//! WebSocket upgrade detection and bidirectional proxying.
//!
//! When a client sends a WebSocket upgrade request (`Connection: Upgrade` +
//! `Upgrade: websocket`), this module forwards the upgrade to the upstream
//! backend and then establishes a bidirectional byte-copy between the client
//! and upstream, effectively tunnelling the WebSocket frames without
//! interpreting them.

use http::{Request, Response, StatusCode};
use hyper::upgrade::OnUpgrade;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use super::activity::BackendActivityGuard;
use super::upstream::ConnGuard;
use super::{connection_header_names, is_hop_by_hop_header};
use crate::{Body, ProxyError, empty_body, websocket};

/// Check whether an incoming request is a WebSocket upgrade.
///
/// Returns `true` when the request contains both `Connection: Upgrade` and
/// `Upgrade: websocket` headers (case-insensitive value comparison).
///
/// Thin wrapper around [`crate::websocket::is_websocket_upgrade`] which takes
/// a `&HeaderMap`; this adapter accepts the full `&Request<B>`.
pub fn is_websocket_upgrade<B>(req: &Request<B>) -> bool {
    websocket::is_websocket_upgrade(req.headers())
}

/// Proxy a WebSocket upgrade request to the given upstream address.
///
/// The flow:
/// 1. Open a raw TCP connection to the upstream.
/// 2. Write the HTTP upgrade request to the upstream.
/// 3. Read the upstream's 101 response.
/// 4. Return a 101 response to the client (with the upgrade extension on the hyper side).
/// 5. Once both sides have upgraded, spawn a task that copies bytes bidirectionally until either
///    side closes.
pub async fn proxy_websocket(
    mut req: Request<Body>,
    upstream_addr: &str,
    conn_guard: ConnGuard,
    activity_guard: BackendActivityGuard,
) -> Result<Response<Body>, ProxyError> {
    // Connect to the upstream over TCP.
    let mut upstream_stream = TcpStream::connect(upstream_addr).await.map_err(|e| {
        ProxyError::Internal(format!(
            "failed to connect to upstream {upstream_addr}: {e}"
        ))
    })?;

    debug!(upstream = %upstream_addr, "connected to upstream for WebSocket upgrade");

    // Build the raw HTTP/1.1 upgrade request to send over the TCP connection.
    let raw_request = build_raw_upgrade_request(&req, upstream_addr);

    // Write the upgrade request to the upstream.
    use tokio::io::AsyncWriteExt;
    upstream_stream
        .write_all(raw_request.as_bytes())
        .await
        .map_err(|e| {
            ProxyError::Internal(format!("failed to write upgrade request to upstream: {e}"))
        })?;

    // Read the upstream's response (enough to see the 101 status line and
    // headers). We read into a buffer until we see the end-of-headers marker
    // (\r\n\r\n).
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        let n = upstream_stream.read(&mut tmp).await.map_err(|e| {
            ProxyError::Internal(format!("failed to read upstream upgrade response: {e}"))
        })?;
        if n == 0 {
            return Err(ProxyError::Internal(
                "upstream closed connection before completing WebSocket handshake".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 16_384 {
            return Err(ProxyError::Internal(
                "upstream upgrade response too large".into(),
            ));
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let response_str = String::from_utf8_lossy(&buf);

    // Verify the upstream responded with 101 Switching Protocols.
    if !response_str.starts_with("HTTP/1.1 101") {
        let first_line = response_str.lines().next().unwrap_or("<empty>");
        warn!(
            upstream = %upstream_addr,
            response = %first_line,
            "upstream did not accept WebSocket upgrade"
        );
        return Err(ProxyError::Internal(format!(
            "upstream did not accept WebSocket upgrade: {first_line}"
        )));
    }

    debug!(upstream = %upstream_addr, "upstream accepted WebSocket upgrade");

    // Build the 101 response to send back to the client. We need to set up
    // the hyper upgrade machinery so we can get the raw IO after sending the
    // response.

    // Capture the client's OnUpgrade before we consume the request.
    // hyper stores the upgrade future in the request extensions.
    let client_upgrade: OnUpgrade = hyper::upgrade::on(&mut req);

    let mut response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "websocket");

    // Forward Sec-WebSocket-Accept and Sec-WebSocket-Protocol from upstream
    // response to the client response.
    for line in response_str.lines().skip(1) {
        if line.is_empty() || line == "\r" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            let name_lower = name.to_ascii_lowercase();
            if name_lower == "sec-websocket-accept"
                || name_lower == "sec-websocket-protocol"
                || name_lower == "sec-websocket-extensions"
            {
                response = response.header(name, value);
            }
        }
    }

    let response = response.body(empty_body())?;

    // Spawn the bidirectional copy task. It will run once the client side
    // completes its upgrade (i.e., after the 101 response is sent).
    tokio::spawn(async move {
        let _conn_guard = conn_guard;
        let _activity_guard = activity_guard;
        match client_upgrade.await {
            Ok(client_io) => {
                let mut client_io = hyper_util::rt::TokioIo::new(client_io);
                let mut upstream_stream = upstream_stream;

                match copy_bidirectional(&mut client_io, &mut upstream_stream).await {
                    Ok((client_to_upstream, upstream_to_client)) => {
                        debug!(
                            client_to_upstream,
                            upstream_to_client, "WebSocket tunnel closed"
                        );
                    }
                    Err(e) => {
                        debug!("WebSocket tunnel error: {e}");
                    }
                }
            }
            Err(e) => {
                error!("WebSocket client upgrade failed: {e}");
            }
        }
    });

    Ok(response)
}

/// Build a raw HTTP/1.1 request string for the WebSocket upgrade, suitable
/// for writing directly to a TCP stream.
fn build_raw_upgrade_request<B>(req: &Request<B>, upstream_addr: &str) -> String {
    let method = req.method();
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let mut raw = format!("{method} {path} HTTP/1.1\r\n");
    raw.push_str(&format!("Host: {upstream_addr}\r\n"));
    raw.push_str("Connection: Upgrade\r\n");
    raw.push_str("Upgrade: websocket\r\n");

    let connection_named_headers = connection_header_names(req.headers());
    for (name, value) in req.headers() {
        if !should_forward_raw_upgrade_header(name, &connection_named_headers) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            raw.push_str(&format!("{}: {v}\r\n", name.as_str()));
        }
    }

    raw.push_str("\r\n");
    raw
}

fn should_forward_raw_upgrade_header(
    name: &http::HeaderName,
    connection_named_headers: &[http::HeaderName],
) -> bool {
    name != http::header::HOST
        && name != http::header::CONNECTION
        && name != http::header::UPGRADE
        && !is_hop_by_hop_header(name)
        && !connection_named_headers.iter().any(|header| header == name)
}

#[cfg(test)]
mod tests {
    use http::header::{CONNECTION, HOST, UPGRADE};

    use super::*;

    #[test]
    fn build_raw_upgrade_request_normalizes_upgrade_headers() {
        let mut req = Request::builder()
            .method("GET")
            .uri("/socket?room=1")
            .body(())
            .unwrap();
        let headers = req.headers_mut();
        headers.insert(HOST, "client.example".parse().unwrap());
        headers.append(CONNECTION, "keep-alive, x-smuggled".parse().unwrap());
        headers.append(CONNECTION, "Upgrade".parse().unwrap());
        headers.insert(UPGRADE, "websocket".parse().unwrap());
        headers.insert("x-smuggled", "secret".parse().unwrap());
        headers.insert("sec-websocket-version", "13".parse().unwrap());
        headers.insert("sec-websocket-key", "test-key".parse().unwrap());

        let raw = build_raw_upgrade_request(&req, "127.0.0.1:3000");

        assert!(raw.starts_with("GET /socket?room=1 HTTP/1.1\r\n"));
        assert!(raw.contains("Host: 127.0.0.1:3000\r\n"));
        assert!(raw.contains("Connection: Upgrade\r\n"));
        assert!(raw.contains("Upgrade: websocket\r\n"));
        assert!(!raw.contains("client.example"));
        assert!(!raw.contains("Connection: keep-alive"));
        assert!(!raw.contains("x-smuggled"));
        assert!(raw.contains("sec-websocket-version: 13\r\n"));
        assert!(raw.contains("sec-websocket-key: test-key\r\n"));
    }
}
