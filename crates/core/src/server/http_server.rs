use std::net::SocketAddr;
use std::sync::Arc;

use hyper::Request;
use hyper::body::Incoming;
use hyper::service::{Service as HyperService, service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;

use super::AppState;

/// Serve a single plain TCP connection using hyper's auto HTTP/1+2 builder.
pub async fn serve_connection(
    stream: tokio::net::TcpStream,
    local_addr: SocketAddr,
    client_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_io(stream, local_addr, client_addr, state, false).await
}

/// Serve a single TLS-wrapped connection using hyper's auto HTTP/1+2 builder.
pub async fn serve_tls_connection(
    stream: TlsStream<tokio::net::TcpStream>,
    local_addr: SocketAddr,
    client_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_io(stream, local_addr, client_addr, state, true).await
}

/// Serve a connection on any I/O type that implements `AsyncRead + AsyncWrite + Unpin`.
///
/// This is the generic entry point used by both plain TCP and TLS connections,
/// as well as connections wrapped in a `PrefixedStream` (when PROXY protocol
/// is enabled).
pub async fn serve_io<IO>(
    io: IO,
    local_addr: SocketAddr,
    client_addr: SocketAddr,
    state: Arc<AppState>,
    is_tls: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(io);

    let service = service_fn(move |req: Request<Incoming>| {
        let state = Arc::clone(&state);
        async move { handle_request(req, local_addr, client_addr, &state, is_tls).await }
    });

    hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .http1()
        .keep_alive(true)
        .timer(TokioTimer::new())
        .http2()
        .timer(TokioTimer::new())
        .serve_connection_with_upgrades(io, service)
        .await?;

    Ok(())
}

async fn handle_request(
    req: Request<Incoming>,
    local_addr: SocketAddr,
    client_addr: SocketAddr,
    state: &AppState,
    is_tls: bool,
) -> Result<hyper::Response<salvo::http::ResBody>, hyper::Error> {
    let service = state.service.load();
    let scheme = if is_tls {
        salvo::http::uri::Scheme::HTTPS
    } else {
        salvo::http::uri::Scheme::HTTP
    };

    #[allow(unused_mut)]
    let mut alt_svc_h3 = None;
    #[cfg(feature = "http3")]
    if is_tls {
        let config = state.config.load();
        if config.global.http3 {
            alt_svc_h3 = format!("h3=\":{}\"; ma=2592000", config.global.https_addr.port())
                .parse()
                .ok();
        }
    }

    let handler = service.hyper_handler(
        local_addr.into(),
        client_addr.into(),
        scheme,
        None,
        alt_svc_h3,
    );
    handler.call(req).await
}
