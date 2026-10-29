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

pub(crate) const POOLED_MIN: usize = 32 * 1024;
const MAX_CHUNK: usize = 64 * 1024;
const MAX_IDLE: usize = 32;
const MAX_IDLE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const STREAM_READ: usize = 128 * 1024;

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
        // Returning the last Bytes clone must not wait for the pool lock.
        // Contention and a full pool simply release the buffer.
        if let Ok(mut idle) = self.pool.idle.try_lock()
            && idle.len() < MAX_IDLE.min(MAX_IDLE_BYTES / self.pool.size)
        {
            idle.push(buffer);
        }
    }
}

static MEDIUM_BUFFERS: BufferPool = BufferPool::new(32 * 1024);
static LARGE_BUFFERS: BufferPool = BufferPool::new(MAX_CHUNK);
static STREAM_BUFFERS: BufferPool = BufferPool::new(STREAM_READ);

/// Reuse initialized buffers for the eager 32–64 KiB response path.
/// The two eager classes retain at most 32 buffers each (3 MiB total).
pub(crate) fn read_bytes(reader: impl Read, length: usize) -> io::Result<bytes::Bytes> {
    assert!((POOLED_MIN..=MAX_CHUNK).contains(&length));
    if length <= MEDIUM_BUFFERS.size {
        MEDIUM_BUFFERS.read(reader, length)
    } else {
        LARGE_BUFFERS.read(reader, length)
    }
}

pub(crate) fn stream(
    reader: impl AsyncRead + Send + Sync + Unpin + 'static,
    chunk: usize,
) -> salvo_core::http::ResBody {
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, chunk);
    let frames = futures_util::StreamExt::map(stream, |chunk| match chunk {
        Ok(bytes) => Ok(http_body::Frame::data(bytes)),
        Err(error) => Err(Box::new(error) as salvo_core::BoxedError),
    });
    salvo_core::http::ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(frames)))
}

static DIAG_NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static DIAG_LOGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn report_delay(event: &str, id: u64, remaining: u64, elapsed: std::time::Duration) {
    if elapsed >= std::time::Duration::from_millis(100)
        && DIAG_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 2000
    {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        eprintln!(
            "FILE_DIAG event={event} id={id} remaining={remaining} elapsed_us={} unix_ms={unix_ms}",
            elapsed.as_micros()
        );
    }
}

pub(crate) fn large_stream(file: std::fs::File, length: u64) -> salvo_core::http::ResBody {
    let id = DIAG_NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let state = (
        Some(file),
        length,
        bytes::Bytes::new(),
        false,
        std::time::Instant::now(),
        id,
    );
    let frames = futures_util::stream::try_unfold(
        state,
        |(mut file, mut remaining, mut ready, started, last, id)| async move {
            if remaining == 0 {
                return Ok::<_, salvo_core::BoxedError>(None);
            }
            report_delay(
                if ready.is_empty() {
                    "poll_gap_empty"
                } else {
                    "poll_gap_ready"
                },
                id,
                remaining,
                last.elapsed(),
            );
            if ready.is_empty() {
                if started {
                    tokio::task::yield_now().await;
                }
                let length = remaining.min(STREAM_READ as u64) as usize;
                let mut input = file.take().expect("a pending batch owns its file");
                let queued = std::time::Instant::now();
                let (next, bytes, finished) = tokio::task::spawn_blocking(move || {
                    report_delay("read_queue", id, remaining, queued.elapsed());
                    let start = std::time::Instant::now();
                    let bytes = STREAM_BUFFERS.read(&mut input, length);
                    report_delay("read_io", id, remaining, start.elapsed());
                    let bytes = bytes?;
                    let file = (remaining > length as u64).then_some(input);
                    Ok::<_, io::Error>((file, bytes, std::time::Instant::now()))
                })
                .await
                .map_err(io::Error::other)??;
                report_delay("read_resume", id, remaining, finished.elapsed());
                file = next;
                ready = bytes;
            }
            let bytes = ready.split_to(MAX_CHUNK.min(ready.len()));
            remaining -= bytes.len() as u64;
            Ok(Some((
                http_body::Frame::data(bytes),
                (file, remaining, ready, true, std::time::Instant::now(), id),
            )))
        },
    );
    salvo_core::http::ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(frames)))
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

    #[test]
    fn dropping_a_lease_does_not_wait_for_the_pool_lock() {
        static POOL: BufferPool = BufferPool::new(64);
        let bytes = POOL.read(&b"ok"[..], 2).unwrap();
        let idle = POOL.idle.lock().unwrap();
        let (dropped, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(bytes);
            dropped.send(()).unwrap();
        });
        // Keep the lock held until Drop completes or the deadline expires.
        // Release it before joining, so a regression fails instead of hanging.
        let completed = done.recv_timeout(std::time::Duration::from_secs(5));
        assert!(idle.is_empty());
        drop(idle);
        worker.join().unwrap();
        assert!(completed.is_ok(), "returning a lease waited for the pool lock");
    }

    #[test]
    fn split_frames_keep_the_batch_lease_until_both_are_released() {
        static POOL: BufferPool = BufferPool::new(128);
        let mut batch = POOL.read(&[b'x'; 128][..], 128).unwrap();
        let first = batch.split_to(64);
        drop(batch);
        assert!(POOL.idle.lock().unwrap().is_empty());
        assert_eq!(first.as_ref(), &[b'x'; 64]);
        drop(first);
        assert_eq!(POOL.idle.lock().unwrap().len(), 1);
    }

    #[test]
    fn large_batches_respect_the_idle_byte_cap() {
        static POOL: BufferPool = BufferPool::new(STREAM_READ);
        let leases: Vec<_> = (0..MAX_IDLE).map(|_| POOL.read(&b"x"[..], 1).unwrap()).collect();
        drop(leases);
        let idle = POOL.idle.lock().unwrap();
        assert_eq!(idle.len(), MAX_IDLE_BYTES / STREAM_READ);
        assert_eq!(idle.iter().map(Vec::len).sum::<usize>(), MAX_IDLE_BYTES);
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
