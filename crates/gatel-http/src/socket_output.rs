//! Apply file-output backpressure without changing proxy or H2 socket policy.
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(crate) struct Output {
    stream: tokio::net::TcpStream,
    #[cfg(target_os = "linux")]
    policy: Option<Policy>,
}

#[cfg(target_os = "linux")]
struct Policy {
    original: u32,
    applied: u32,
    file: gatel_plan::BoundedTcpOutput,
}

pub(crate) fn configure(stream: tokio::net::TcpStream) -> (Output, Option<gatel_plan::BoundedTcpOutput>) {
    #[cfg(target_os = "linux")]
    {
        let socket = socket2::SockRef::from(&stream);
        let policy = socket.tcp_notsent_lowat().ok().and_then(|original| {
            socket.set_tcp_notsent_lowat(64 * 1024).ok()?;
            Some(Policy {
                original,
                applied: 64 * 1024,
                file: gatel_plan::BoundedTcpOutput::default(),
            })
        });
        let hint = policy.as_ref().map(|policy| policy.file.clone());
        (Output { stream, policy }, hint)
    }
    #[cfg(not(target_os = "linux"))]
    (Output { stream }, None)
}

impl Output {
    fn apply(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(policy) = &mut self.policy {
            let desired = if policy.file.enabled() {
                64 * 1024
            } else {
                policy.original
            };
            if desired != policy.applied {
                // Leave SO_SNDBUF and bytes in flight alone. Restore the
                // previous policy for the next non-file HTTP/1 response.
                socket2::SockRef::from(&self.stream).set_tcp_notsent_lowat(desired)?;
                policy.applied = desired;
            }
        }
        Ok(())
    }
}

impl AsyncRead for Output {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for Output {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        self.apply()?;
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.apply()?;
        Pin::new(&mut self.stream).poll_write_vectored(cx, bytes)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWrite as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn output_round_trips_and_preserves_vectored_writes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let vectored = stream.is_write_vectored();
        let (mut output, hint) = super::configure(stream);
        assert_eq!(output.is_write_vectored(), vectored);
        let parts = [
            std::io::IoSlice::new(b"plain"),
            std::io::IoSlice::new(b" response"),
        ];
        assert_eq!(output.write_vectored(&parts).await.unwrap(), 14);
        let mut bytes = [0; 14];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"plain response");
        client.write_all(b"request").await.unwrap();
        let mut bytes = [0; 7];
        output.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"request");
        #[cfg(not(target_os = "linux"))]
        assert!(hint.is_none());
        #[cfg(target_os = "linux")]
        assert!(hint.is_some());
        output.shutdown().await.unwrap();
        assert_eq!(client.read(&mut bytes).await.unwrap(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn file_then_proxy_restores_the_original_policy_without_capping_the_send_buffer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let socket = socket2::SockRef::from(&stream);
        // Preserve a configured value, not just the system default.
        socket.set_tcp_notsent_lowat(256 * 1024).unwrap();
        let send_buffer = socket.send_buffer_size().unwrap();
        let (mut output, hint) = super::configure(stream);
        let hint = hint.unwrap();
        for file in [false, true, false, true, false] {
            hint.reset();
            if file {
                hint.enable();
            }
            output.write_all(b"response").await.unwrap();
            let socket = socket2::SockRef::from(&output.stream);
            assert_eq!(
                socket.tcp_notsent_lowat().unwrap(),
                if file { 64 * 1024 } else { 256 * 1024 }
            );
            assert_eq!(socket.send_buffer_size().unwrap(), send_buffer);
        }
    }
}
