//! Owned, bounded reads without a second file buffer or an unbounded producer.
use std::fs::File;
use std::future::Future;
use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::task::JoinHandle;

// Independent of the 64 KiB eager-read threshold. One 128 KiB outgoing buffer
// replaces the old 64 KiB Tokio file buffer plus 64 KiB ReaderStream buffer.
const CHUNK: usize = 128 * 1024;

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

type ReadJob = JoinHandle<io::Result<(Option<File>, Vec<u8>)>>;

pub(crate) struct FileBody {
    file: Option<File>,
    pending: Option<ReadJob>,
    remaining: u64,
}

impl FileBody {
    pub(crate) fn new(file: File, length: u64) -> Self {
        Self {
            file: Some(file),
            pending: None,
            remaining: length,
        }
    }
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        if this.pending.is_none() {
            let mut file = this.file.take().expect("an idle file body owns its reader");
            let remaining = this.remaining;
            let length = remaining.min(CHUNK as u64) as usize;
            this.pending = Some(tokio::task::spawn_blocking(move || {
                let bytes = read_exact_prefix(&mut file, length)?;
                // Close the last reader in the worker too. Dropping the body
                // while a read is pending drops its eventual owned result.
                let file = (remaining > length as u64).then_some(file);
                Ok((file, bytes))
            }));
        }
        let result = std::task::ready!(Pin::new(this.pending.as_mut().expect("read was started")).poll(cx));
        this.pending = None;
        match result.map_err(io::Error::other).and_then(|result| result) {
            Ok((file, bytes)) => {
                this.remaining -= bytes.len() as u64;
                this.file = file;
                Poll::Ready(Some(Ok(Frame::data(bytes.into()))))
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
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(std::path::PathBuf);
    impl Temporary {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gatel-file-body-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
        fn open(&self) -> File {
            File::open(&self.0).unwrap()
        }
        fn truncate(&self, length: u64) {
            File::options()
                .write(true)
                .open(&self.0)
                .unwrap()
                .set_len(length)
                .unwrap();
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
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
    async fn stream_reads_on_demand_and_fuses_a_failure_before_the_first_frame() {
        let source = Temporary::new(&vec![b'x'; CHUNK + 1]);
        let mut body = FileBody::new(source.open(), (CHUNK + 1) as u64);
        source.truncate(0);
        let error = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn stream_stops_at_selected_length_after_the_file_grows() {
        let selected = CHUNK * 2 + 7;
        let source = Temporary::new(&vec![b'x'; selected]);
        let mut body = FileBody::new(source.open(), selected as u64);
        use std::io::Write;
        File::options()
            .append(true)
            .open(&source.0)
            .unwrap()
            .write_all(&vec![b'z'; CHUNK])
            .unwrap();
        let mut received = 0;
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let bytes = frame.unwrap().into_data().unwrap();
            assert!(bytes.len() <= CHUNK);
            assert!(bytes.iter().all(|byte| *byte == b'x'));
            received += bytes.len();
            assert_eq!(body.size_hint().exact(), Some((selected - received) as u64));
        }
        assert_eq!(received, selected);
        assert!(body.is_end_stream());
    }

    #[tokio::test]
    async fn stream_bounds_frames_and_reports_truncation_between_reads() {
        let source = Temporary::new(&vec![b'x'; CHUNK * 2 + 1]);
        let mut body = FileBody::new(source.open(), (CHUNK * 2 + 1) as u64);
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first.len(), CHUNK);
        assert_eq!(body.size_hint().exact(), Some((CHUNK + 1) as u64));
        source.truncate((CHUNK + 5) as u64);
        let error = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(body.is_end_stream());
        assert!(
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .is_none()
        );
    }
}
