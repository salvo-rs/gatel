//! A bounded file reader must distinguish early EOF from a complete response.
use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf, Take};

pub(crate) fn read_exact_prefix(reader: impl Read, length: usize) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(length);
    reader.take(length as u64).read_to_end(&mut buffer)?;
    if buffer.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file ended before its selected length",
        ));
    }
    Ok(buffer)
}

const POOLED_MIN: usize = 32 * 1024;
const MAX_CHUNK: usize = 64 * 1024;
const MAX_IDLE: usize = 32;

struct BufferPool {
    size: usize,
    idle: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl BufferPool {
    const fn new(size: usize) -> Self {
        Self {
            size,
            idle: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn read(&'static self, mut reader: impl Read, length: usize) -> io::Result<bytes::Bytes> {
        assert!(length <= self.size);
        // Do not hold the pool lock during allocation or filesystem I/O.
        let buffer = self
            .idle
            .try_lock()
            .ok()
            .and_then(|mut idle| idle.pop())
            .unwrap_or_else(|| vec![0; self.size]);
        let mut lease = BufferLease {
            pool: self,
            buffer: Some(buffer),
            length: 0,
        };
        reader.read_exact(&mut lease.buffer.as_mut().expect("a live lease owns its buffer")[..length])?;
        lease.length = length;
        Ok(bytes::Bytes::from_owner(lease))
    }
}

struct BufferLease {
    pool: &'static BufferPool,
    buffer: Option<Vec<u8>>,
    length: usize,
}

impl AsRef<[u8]> for BufferLease {
    fn as_ref(&self) -> &[u8] {
        &self.buffer.as_ref().expect("a live lease owns its buffer")[..self.length]
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        let Some(buffer) = self.buffer.take() else { return };
        // A network task dropping the last Bytes clone must never wait for
        // another reader. Contention and a full pool simply release the buffer.
        if let Ok(mut idle) = self.pool.idle.try_lock()
            && idle.len() < MAX_IDLE
        {
            idle.push(buffer);
        }
    }
}

static MEDIUM_BUFFERS: BufferPool = BufferPool::new(32 * 1024);
static LARGE_BUFFERS: BufferPool = BufferPool::new(MAX_CHUNK);

/// Reuse only initialized buffers, and expose bytes only after an exact read.
/// Small tails retain the existing allocation/read path. Idle retention is
/// bounded to 32 buffers per class (3 MiB total), independently of live bodies.
pub(crate) fn read_bytes(reader: impl Read, length: usize) -> io::Result<bytes::Bytes> {
    if length < POOLED_MIN {
        return read_exact_prefix(reader, length).map(Into::into);
    }
    if length <= MEDIUM_BUFFERS.size {
        MEDIUM_BUFFERS.read(reader, length)
    } else {
        LARGE_BUFFERS.read(reader, length)
    }
}

type ReadJob = tokio::task::JoinHandle<io::Result<(Option<std::fs::File>, bytes::Bytes)>>;

/// A demand-driven file body with one bounded read in flight. The outgoing
/// Bytes owns its buffer lease until its last clone is released by the consumer.
pub(crate) struct FileBody {
    file: Option<std::fs::File>,
    pending: Option<ReadJob>,
    remaining: u64,
    chunk: usize,
}

impl FileBody {
    pub(crate) fn new(file: std::fs::File, length: u64, chunk: usize) -> Self {
        assert!(chunk > 0 && chunk <= MAX_CHUNK);
        Self {
            file: Some(file),
            pending: None,
            remaining: length,
            chunk,
        }
    }
}

impl http_body::Body for FileBody {
    type Data = bytes::Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::future::Future;
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        if this.pending.is_none() {
            let mut file = this.file.take().expect("an idle body owns its file");
            let remaining = this.remaining;
            let length = remaining.min(this.chunk as u64) as usize;
            this.pending = Some(tokio::task::spawn_blocking(move || {
                let bytes = read_bytes(&mut file, length)?;
                let file = (remaining > length as u64).then_some(file);
                Ok((file, bytes))
            }));
        }
        let result = std::task::ready!(Pin::new(this.pending.as_mut().expect("read started")).poll(cx));
        this.pending = None;
        match result.map_err(io::Error::other).and_then(|result| result) {
            Ok((file, bytes)) => {
                this.remaining -= bytes.len() as u64;
                this.file = file;
                Poll::Ready(Some(Ok(http_body::Frame::data(bytes))))
            }
            Err(error) => {
                this.remaining = 0;
                this.file = None;
                Poll::Ready(Some(Err(Box::new(error))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.remaining)
    }
}

pub(crate) struct ExactReader<R> {
    inner: Take<R>,
}

impl<R: AsyncRead> ExactReader<R> {
    pub(crate) fn new(reader: R, length: u64) -> Self {
        Self {
            inner: reader.take(length),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ExactReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 || self.inner.limit() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) if buffer.filled().len() == before => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file ended before its selected length",
            ))),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooled_bytes_keep_the_lease_until_the_last_clone_is_dropped() {
        static POOL: BufferPool = BufferPool::new(64);
        let bytes = POOL.read(&b"original"[..], 8).unwrap();
        let clone = bytes.clone();
        drop(bytes);
        assert!(POOL.idle.lock().unwrap().is_empty());
        assert_eq!(clone.as_ref(), b"original");
        drop(clone);
        assert_eq!(POOL.idle.lock().unwrap().len(), 1);
        let reused = POOL.read(&b"new"[..], 3).unwrap();
        assert_eq!(reused.as_ref(), b"new");
        assert!(POOL.idle.lock().unwrap().is_empty());
    }

    #[test]
    fn a_short_read_returns_its_lease_without_exposing_old_bytes() {
        static POOL: BufferPool = BufferPool::new(64);
        drop(POOL.read(&[b's'; 64][..], 64).unwrap());
        assert_eq!(
            POOL.read(&b"short"[..], 64).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(POOL.idle.lock().unwrap().len(), 1);
        assert_eq!(POOL.read(&b"x"[..], 1).unwrap().as_ref(), b"x");
    }

    #[test]
    fn idle_retention_is_bounded_even_after_a_burst() {
        static POOL: BufferPool = BufferPool::new(64);
        let leases: Vec<_> = (0..MAX_IDLE + 5)
            .map(|_| POOL.read(&b"ok"[..], 2).unwrap())
            .collect();
        assert!(POOL.idle.lock().unwrap().is_empty());
        drop(leases);
        let idle = POOL.idle.lock().unwrap();
        assert_eq!(idle.len(), MAX_IDLE);
        assert!(idle.iter().all(|buffer| buffer.len() == 64));
    }

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gatel-file-lease-{}-{id}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }

        fn open(&self) -> std::fs::File {
            std::fs::File::open(&self.0).unwrap()
        }

        fn truncate(&self, length: u64) {
            std::fs::File::options()
                .write(true)
                .open(&self.0)
                .unwrap()
                .set_len(length)
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn streamed_reads_are_lazy_and_fuse_an_initial_error() {
        use http_body::Body as _;
        use http_body_util::BodyExt as _;
        let fixture = Fixture::new(&vec![b'x'; MAX_CHUNK + 1]);
        let mut body = FileBody::new(fixture.open(), (MAX_CHUNK + 1) as u64, MAX_CHUNK);
        fixture.truncate(0);
        let error = body.frame().await.unwrap().unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn streamed_frames_bound_growth_and_report_exact_remaining_bytes() {
        use std::io::Write as _;

        use http_body::Body as _;
        use http_body_util::BodyExt as _;
        let length = MAX_CHUNK * 2 + 17;
        let selected: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
        let fixture = Fixture::new(&selected);
        let mut body = FileBody::new(fixture.open(), length as u64, MAX_CHUNK);
        std::fs::File::options()
            .append(true)
            .open(&fixture.0)
            .unwrap()
            .write_all(b"unselected tail")
            .unwrap();
        let mut received = Vec::new();
        while let Some(frame) = body.frame().await {
            let bytes = frame.unwrap().into_data().unwrap();
            assert!(!bytes.is_empty() && bytes.len() <= MAX_CHUNK);
            received.extend_from_slice(&bytes);
            assert_eq!(body.size_hint().exact(), Some((length - received.len()) as u64));
        }
        assert_eq!(received, selected);
        assert!(body.is_end_stream());
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn truncation_between_frames_is_not_a_successful_short_body() {
        use http_body_util::BodyExt as _;
        let fixture = Fixture::new(&vec![b'x'; MAX_CHUNK * 2]);
        let mut body = FileBody::new(fixture.open(), (MAX_CHUNK * 2) as u64, MAX_CHUNK);
        assert_eq!(
            body.frame().await.unwrap().unwrap().data_ref().unwrap().len(),
            MAX_CHUNK
        );
        fixture.truncate(MAX_CHUNK as u64);
        let error = body.frame().await.unwrap().unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(body.frame().await.is_none());
    }
    #[test]
    fn bounded_read_rejects_short_input_and_ignores_growth() {
        assert_eq!(
            read_exact_prefix(b"short".as_slice(), 10).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(read_exact_prefix(b"longer".as_slice(), 4).unwrap(), b"long");
    }

    #[tokio::test]
    async fn exact_reader_rejects_short_input_and_bounds_growth() {
        let mut reader = ExactReader::new(b"short".as_slice(), 10);
        let mut output = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut output).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(output, b"short");
        let mut reader = ExactReader::new(b"longer".as_slice(), 4);
        output.clear();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"long");
    }
}
