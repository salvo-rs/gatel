//! Apply file-output backpressure without changing proxy or H2 socket policy.
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// Bound unsent bulk-file bytes without capping SO_SNDBUF or bytes in flight.
#[cfg(target_os = "linux")]
const FILE_LOWAT: u32 = 64 * 1024;

pub(crate) struct Output {
    stream: tokio::net::TcpStream,
    #[cfg(target_os = "linux")]
    policy: Option<Policy>,
}

#[cfg(target_os = "linux")]
struct Policy {
    original: u32,
    applied: u32,
    hint: Arc<Hint>,
}

#[derive(Default)]
pub(crate) struct Hint(AtomicBool);

impl Hint {
    pub fn set_file(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }
}

pub(crate) fn configure(stream: tokio::net::TcpStream) -> (Output, Option<Arc<Hint>>) {
    #[cfg(target_os = "linux")]
    {
        let socket = socket2::SockRef::from(&stream);
        let policy = socket.tcp_notsent_lowat().ok().and_then(|original| {
            // Check setter support without changing the current policy.
            socket.set_tcp_notsent_lowat(original).ok()?;
            Some(Policy {
                original,
                applied: original,
                hint: Arc::new(Hint::default()),
            })
        });
        let hint = policy.as_ref().map(|policy| Arc::clone(&policy.hint));
        (Output { stream, policy }, hint)
    }
    #[cfg(not(target_os = "linux"))]
    (Output { stream }, None)
}

impl Output {
    fn apply(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(policy) = &mut self.policy {
            let desired = if policy.hint.0.load(Ordering::Relaxed) {
                FILE_LOWAT
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn a_file_output_hint_is_zero_sized_and_survives_response_conversion() {
        assert_eq!(std::mem::size_of::<gatel_plan::BoundedFileOutput>(), 0);
        let mut response = salvo_core::http::Response::new();
        response.extensions.insert(gatel_plan::BoundedFileOutput);
        assert!(
            response
                .into_hyper()
                .extensions()
                .get::<gatel_plan::BoundedFileOutput>()
                .is_some()
        );
    }

    use super::*;

    #[tokio::test]
    async fn io_contract_and_owner_cancellation_do_not_depend_on_output_hints() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let vectored = stream.is_write_vectored();
        let (mut output, _hint) = configure(stream);
        assert_eq!(output.is_write_vectored(), vectored);
        let work = crate::connection_work::ConnectionWork::default();
        let (held, closed) = tokio::sync::oneshot::channel::<()>();
        work.set(async move {
            let _held = held;
            std::future::pending::<()>().await;
        })
        .unwrap();
        drop(work);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), closed)
                .await
                .unwrap()
                .is_err()
        );
        let parts = [io::IoSlice::new(b"plain"), io::IoSlice::new(b" response")];
        assert_eq!(output.write_vectored(&parts).await.unwrap(), 14);
        let mut data = [0; 14];
        client.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"plain response");
        client.write_all(b"request").await.unwrap();
        let mut data = [0; 7];
        output.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"request");
        output.shutdown().await.unwrap();
        assert_eq!(client.read(&mut data).await.unwrap(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn file_policy_restores_the_original_lowat_and_never_caps_the_send_buffer() {
        for original in [0, 256 * 1024] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let socket = socket2::SockRef::from(&stream);
            socket.set_tcp_notsent_lowat(original).unwrap();
            let send_buffer = socket.send_buffer_size().unwrap();
            let (mut output, hint) = configure(stream);
            let hint = hint.unwrap();
            assert_eq!(
                socket2::SockRef::from(&output.stream)
                    .tcp_notsent_lowat()
                    .unwrap(),
                original
            );
            for file in [false, true, false, true, false] {
                hint.set_file(file);
                output.write_all(b"response").await.unwrap();
                let socket = socket2::SockRef::from(&output.stream);
                assert_eq!(
                    socket.tcp_notsent_lowat().unwrap(),
                    if file { FILE_LOWAT } else { original }
                );
                assert_eq!(socket.send_buffer_size().unwrap(), send_buffer);
            }
        }
    }
}
