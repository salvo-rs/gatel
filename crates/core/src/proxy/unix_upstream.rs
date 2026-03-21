//! Unix domain socket upstream support.
//!
//! Allows proxying requests to backends listening on Unix sockets instead of
//! TCP. The upstream address is specified as `unix:/path/to/socket` in the
//! configuration.
//!
//! This module is only available on Unix platforms.

#![cfg(unix)]

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::rt::{Read, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

/// A connector that establishes connections over Unix domain sockets.
#[derive(Clone)]
pub struct UnixConnector {
    path: String,
}

impl UnixConnector {
    /// Create a connector for the given socket path.
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

/// Wrapper around `TokioIo<UnixStream>` that implements `Connection`.
pub struct UnixConnection(TokioIo<UnixStream>);

impl Connection for UnixConnection {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl Read for UnixConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl Write for UnixConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl hyper::service::Service<http::Uri> for UnixConnector {
    type Response = UnixConnection;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let path = self.path.clone();
        Box::pin(async move {
            let stream = UnixStream::connect(&path).await?;
            Ok(UnixConnection(TokioIo::new(stream)))
        })
    }
}

/// Returns `true` if the address looks like a Unix socket path.
pub fn is_unix_addr(addr: &str) -> bool {
    addr.starts_with("unix:") || addr.starts_with("/")
}

/// Extract the socket path from a `unix:/path/to/sock` address.
pub fn parse_unix_path(addr: &str) -> &str {
    addr.strip_prefix("unix:").unwrap_or(addr)
}

/// Build an HTTP client that connects over a Unix domain socket.
pub fn build_unix_client(
    socket_path: &str,
) -> hyper_util::client::legacy::Client<UnixConnector, crate::Body> {
    let connector = UnixConnector::new(socket_path);
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(connector)
}
