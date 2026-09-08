//! Active health probes.
//!
//! A probe opens its own connection every time. It deliberately does not use
//! the business pool: a probe that reused a pooled connection would report the
//! backend healthy because a *previous* request had worked, and would consume a
//! slot that a real request wants.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use gatel_plan::{HealthKind, UpstreamProtocol};
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;

use crate::pool::Body;

/// Probe timeout/caller cancellation must also close its HTTP driver socket.
struct Driver<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for Driver<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Why a probe failed. Kept as a string because nothing branches on it: a
/// failed probe is a failed probe, and the text is for the operator.
pub type ProbeError = String;

/// Run one probe against one endpoint.
///
/// The timeout covers everything — connect, request and response head — because
/// a backend that accepts a connection and then goes quiet is not healthy, and
/// splitting the budget would let it stall for the sum of the parts.
pub async fn probe(
    address: SocketAddr,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    timeout: Duration,
) -> Result<(), ProbeError> {
    probe_with_tls(address, kind, protocol, timeout, None).await
}

pub(crate) async fn probe_with_tls(
    address: SocketAddr,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    timeout: Duration,
    tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), ProbeError> {
    match tokio::time::timeout(timeout, run(address, kind, protocol, tls)).await {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {timeout:?}")),
    }
}

/// Run one probe against a Unix domain socket.
///
/// The same check, over a different kind of socket. A backend behind a socket
/// path is as capable of being unhealthy as one behind an address, and a
/// gateway that could not check it would have to guess.
#[cfg(unix)]
pub async fn probe_unix(
    path: &std::path::Path,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    timeout: Duration,
) -> Result<(), ProbeError> {
    probe_unix_with_tls(path, kind, protocol, timeout, None).await
}

#[cfg(unix)]
pub(crate) async fn probe_unix_with_tls(
    path: &std::path::Path,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    timeout: Duration,
    tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), ProbeError> {
    let run = async {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|error| error.to_string())?;
        // A filesystem path is not an HTTP authority (especially in H2).
        secure_check(stream, kind, protocol, "localhost", tls).await
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {timeout:?}")),
    }
}

#[cfg(not(unix))]
pub async fn probe_unix(
    path: &std::path::Path,
    _kind: &HealthKind,
    _protocol: UpstreamProtocol,
    _timeout: Duration,
) -> Result<(), ProbeError> {
    Err(format!(
        "{}: Unix sockets need a Unix-like operating system",
        path.display()
    ))
}

#[cfg(not(unix))]
pub(crate) async fn probe_unix_with_tls(
    path: &std::path::Path,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    timeout: Duration,
    _tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), ProbeError> {
    probe_unix(path, kind, protocol, timeout).await
}

async fn run(
    address: SocketAddr,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), ProbeError> {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|error| error.to_string())?;
    let _ = stream.set_nodelay(true);
    secure_check(stream, kind, protocol, &address.to_string(), tls).await
}

async fn secure_check<IO>(
    stream: IO,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    authority: &str,
    tls: Option<&gatel_tls::ClientTls>,
) -> Result<(), ProbeError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // TCP-connect is deliberately transport-only, even for a TLS upstream.
    if matches!(kind, HealthKind::TcpConnect) {
        return Ok(());
    }
    match tls {
        Some(tls) => check(tls.connect(stream).await?, kind, protocol, authority, "https").await,
        None => check(stream, kind, protocol, authority, "http").await,
    }
}

/// The check itself, over whatever kind of socket got here.
///
/// `authority` is what goes in the `Host` header when the configuration did
/// not say — the socket address for TCP, and `localhost` for Unix sockets.
async fn check<IO>(
    stream: IO,
    kind: &HealthKind,
    protocol: UpstreamProtocol,
    authority: &str,
    scheme: &str,
) -> Result<(), ProbeError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let HealthKind::Http { path, status, host } = kind else {
        // A TCP probe has already learned what it can: the backend accepted a
        // connection. Closing it immediately is the whole check.
        return Ok(());
    };

    let authority = host.clone().unwrap_or_else(|| authority.to_string());
    let uri = match protocol {
        UpstreamProtocol::Http1 => path.parse::<http::Uri>(),
        UpstreamProtocol::Http2 => format!("{scheme}://{authority}{path}").parse::<http::Uri>(),
    }
    .map_err(|error| error.to_string())?;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .header(http::header::HOST, authority)
        // A probe is not a browser and should not be served a cached page.
        .header(http::header::CACHE_CONTROL, "no-cache")
        .body(empty_body())
        .map_err(|error| error.to_string())?;

    let response = match protocol {
        UpstreamProtocol::Http1 => {
            let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|error| error.to_string())?;
            // The driver ends when the sender is dropped, which happens as soon
            // as this function returns; a probe never leaves a task behind.
            let driver = Driver(tokio::spawn(connection));
            let response = sender
                .send_request(request)
                .await
                .map_err(|error| error.to_string());
            drop(driver);
            response?
        }
        UpstreamProtocol::Http2 => {
            let (mut sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .map_err(|error| error.to_string())?;
            let driver = Driver(tokio::spawn(connection));
            let response = sender
                .send_request(request)
                .await
                .map_err(|error| error.to_string());
            drop(driver);
            response?
        }
    };

    let actual = response.status().as_u16();
    if actual == *status {
        Ok(())
    } else {
        Err(format!("expected {status}, got {actual}"))
    }
}

fn empty_body() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

#[cfg(test)]
#[path = "probe_cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(all(test, unix))]
#[path = "probe_unix_authority_tests.rs"]
mod unix_authority_tests;

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A backend that answers every request with one status.
    async fn http_backend(status: u16) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = stream.read(&mut buffer).await {
                        if read == 0 {
                            return;
                        }
                        let response = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        address
    }

    /// A socket that accepts and then says nothing.
    async fn silent_backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        address
    }

    fn closed_address() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    /// Long enough that a working backend answers, short enough that a broken
    /// one does not hold the suite up.
    #[cfg(unix)]
    const SOON: Duration = Duration::from_secs(2);

    fn http(path: &str, status: u16) -> HealthKind {
        HealthKind::Http {
            path: path.to_string(),
            status,
            host: None,
        }
    }

    #[tokio::test]
    async fn a_tcp_probe_only_asks_whether_the_port_is_open() {
        let open = silent_backend().await;
        assert!(
            probe(
                open,
                &HealthKind::TcpConnect,
                UpstreamProtocol::Http1,
                Duration::from_secs(1)
            )
            .await
            .is_ok(),
            "accepting the connection is the whole check"
        );

        let closed = closed_address();
        assert!(
            probe(
                closed,
                &HealthKind::TcpConnect,
                UpstreamProtocol::Http1,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn an_http_probe_compares_the_exact_status() {
        let backend = http_backend(200).await;
        let timeout = Duration::from_secs(2);

        assert!(
            probe(backend, &http("/healthz", 200), UpstreamProtocol::Http1, timeout)
                .await
                .is_ok()
        );

        let error = probe(backend, &http("/healthz", 204), UpstreamProtocol::Http1, timeout)
            .await
            .unwrap_err();
        assert!(error.contains("expected 204, got 200"), "{error}");
    }

    #[tokio::test]
    async fn an_unhealthy_status_fails_the_probe() {
        let backend = http_backend(503).await;
        let error = probe(
            backend,
            &http("/healthz", 200),
            UpstreamProtocol::Http1,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(error.contains("got 503"), "{error}");
    }

    #[tokio::test]
    async fn a_backend_that_accepts_and_stalls_is_not_healthy() {
        // The failure mode a plain TCP probe cannot see.
        let stalled = silent_backend().await;
        let error = probe(
            stalled,
            &http("/healthz", 200),
            UpstreamProtocol::Http1,
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        assert!(error.contains("no answer within"), "{error}");
    }

    #[tokio::test]
    async fn the_timeout_covers_connecting_too() {
        let error = probe(
            closed_address(),
            &http("/healthz", 200),
            UpstreamProtocol::Http1,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(!error.is_empty());
    }

    // -- Unix domain sockets -------------------------------------------------
    //
    // The same checks, over a socket path. A backend behind one is as capable
    // of being unhealthy as one behind an address, and probing only addresses
    // would have marked every socket endpoint permanently unhealthy.

    #[cfg(unix)]
    mod unix {
        use tokio::net::UnixListener;

        use super::*;

        fn socket(tag: &str) -> std::path::PathBuf {
            let path = std::env::temp_dir().join(format!("gatel-probe-{tag}-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            path
        }

        async fn backend(path: std::path::PathBuf, status: u16) {
            let listener = UnixListener::bind(&path).unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 4096];
                        while let Ok(read) = stream.read(&mut buffer).await {
                            if read == 0 {
                                return;
                            }
                            let answer = format!(
                                "HTTP/1.1 {status} X
Content-Length: 0

"
                            );
                            if stream.write_all(answer.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
        }

        #[tokio::test]
        async fn a_tcp_style_probe_over_a_socket_is_just_connecting() {
            let path = socket("connect");
            backend(path.clone(), 200).await;
            assert!(
                probe_unix(&path, &HealthKind::TcpConnect, UpstreamProtocol::Http1, SOON)
                    .await
                    .is_ok()
            );
        }

        #[tokio::test]
        async fn an_http_probe_over_a_socket_checks_the_status() {
            let path = socket("http");
            backend(path.clone(), 200).await;
            assert!(
                probe_unix(&path, &http("/healthz", 200), UpstreamProtocol::Http1, SOON)
                    .await
                    .is_ok()
            );

            let wrong = probe_unix(&path, &http("/healthz", 204), UpstreamProtocol::Http1, SOON)
                .await
                .unwrap_err();
            assert!(wrong.contains("expected 204, got 200"), "{wrong}");
        }

        #[tokio::test]
        async fn a_socket_that_is_not_there_fails_rather_than_hangs() {
            let path = socket("absent");
            assert!(
                probe_unix(&path, &HealthKind::TcpConnect, UpstreamProtocol::Http1, SOON)
                    .await
                    .is_err()
            );
        }
    }
}
