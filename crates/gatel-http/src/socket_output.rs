//! Bound queued, unsent TCP bytes without capping the send buffer or flight size.

#[cfg(target_os = "linux")]
pub(crate) fn configure(stream: &tokio::net::TcpStream) -> bool {
    // This changes when writable readiness resumes, preserving TCP's send-buffer
    // auto-tuning. A total SO_SNDBUF cap would limit throughput on distant links.
    // Unsupported kernels retain the portable file reader instead.
    socket2::SockRef::from(stream)
        .set_tcp_notsent_lowat(64 * 1024)
        .is_ok()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn configure(_stream: &tokio::net::TcpStream) -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn configuring_output_preserves_the_send_buffer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let socket = socket2::SockRef::from(&stream);
        let send_buffer = socket.send_buffer_size().unwrap();
        let configured = super::configure(&stream);
        assert_eq!(socket.send_buffer_size().unwrap(), send_buffer);
        #[cfg(target_os = "linux")]
        {
            assert!(configured);
            assert_eq!(socket.tcp_notsent_lowat().unwrap(), 64 * 1024);
        }
        #[cfg(not(target_os = "linux"))]
        assert!(!configured);
        drop(client);
    }
}
