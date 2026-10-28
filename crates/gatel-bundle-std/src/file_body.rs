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
