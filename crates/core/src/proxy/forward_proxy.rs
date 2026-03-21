//! HTTP CONNECT forward proxy support.
//!
//! When a client sends a CONNECT request, this handler connects to the target
//! host:port and sets up a bidirectional tunnel between the client and target.
//! This allows clients to proxy HTTPS and other TCP connections.
//!
//! ## HTTP/1.1 CONNECT
//!
//! The standard flow: client sends `CONNECT host:port HTTP/1.1`, the server
//! responds with `200 Connection Established`, and then the connection is
//! upgraded to a raw TCP tunnel via `hyper::upgrade::on`.
//!
//! ## HTTP/2 CONNECT (RFC 7540 §8.3 / RFC 8441 Extended CONNECT)
//!
//! In HTTP/2, CONNECT is carried over a single H2 stream using the `:method`
//! pseudo-header. hyper routes HTTP/2 CONNECT requests through the same
//! `Method::CONNECT` code path so the method detection below works for both
//! versions.
//!
//! However, HTTP/2 does not use the `hyper::upgrade` mechanism — the H2
//! stream itself is the tunnel. Raw per-stream byte access is not directly
//! exposed by hyper's HTTP/2 server API, so full bidirectional tunneling over
//! HTTP/2 CONNECT (as defined by RFC 8441 Extended CONNECT) is not supported
//! at this time. HTTP/2 CONNECT requests will receive a 200 response but the
//! tunnel will not carry data; clients should fall back to HTTP/1.1 CONNECT
//! for tunneling.

use http::{Response, StatusCode, Version};
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tracing::{debug, error, warn};

use crate::config::BasicAuthUser;
use crate::{Body, ProxyError, empty_body, full_body, goals};

/// Forward proxy handler: supports HTTP CONNECT tunneling.
///
/// When a client sends `CONNECT host:port HTTP/1.1`, this handler:
/// 1. Optionally verifies `Proxy-Authorization: Basic` credentials.
/// 2. Connects to the target host:port over TCP.
/// 3. Responds with `200 Connection Established`.
/// 4. Upgrades the client connection and copies bytes bidirectionally.
pub struct ForwardProxy {
    auth_users: Vec<ProxyAuthUser>,
}

struct ProxyAuthUser {
    username: String,
    password_hash: String,
    is_bcrypt: bool,
}

impl ForwardProxy {
    pub fn new(auth_users: &[BasicAuthUser]) -> Self {
        let auth_users = auth_users
            .iter()
            .map(|u| {
                let is_bcrypt = u.password_hash.starts_with("$2b$")
                    || u.password_hash.starts_with("$2a$")
                    || u.password_hash.starts_with("$2y$");
                ProxyAuthUser {
                    username: u.username.clone(),
                    password_hash: u.password_hash.clone(),
                    is_bcrypt,
                }
            })
            .collect();
        Self { auth_users }
    }
}

#[salvo::async_trait]
impl salvo::Handler for ForwardProxy {
    async fn handle(
        &self,
        req: &mut salvo::Request,
        _depot: &mut salvo::Depot,
        res: &mut salvo::Response,
        ctrl: &mut salvo::FlowCtrl,
    ) {
        let client_addr = crate::hoops::client_addr(req);
        let request = match goals::strip_request(req) {
            Ok(r) => r,
            Err(e) => {
                goals::merge_response(res, e.into_response());
                ctrl.skip_rest();
                return;
            }
        };
        let response = self
            .run(request, client_addr)
            .await
            .unwrap_or_else(|e| e.into_response());
        goals::merge_response(res, response);
        ctrl.skip_rest();
    }
}

impl ForwardProxy {
    async fn run(
        &self,
        mut request: http::Request<Body>,
        _client_addr: std::net::SocketAddr,
    ) -> Result<Response<Body>, ProxyError> {
        if request.method() != http::Method::CONNECT {
            return Err(ProxyError::BadRequest(
                "forward proxy only supports CONNECT".into(),
            ));
        }

        // Enforce proxy authentication when auth_users is configured.
        if !self.auth_users.is_empty() {
            match extract_proxy_credentials(&request) {
                Some((username, password)) => {
                    let ok = self
                        .auth_users
                        .iter()
                        .any(|u| verify_proxy_user(u, &username, &password));
                    if !ok {
                        debug!(
                            username = username.as_str(),
                            "proxy authentication failed, returning 407"
                        );
                        return Ok(proxy_auth_required_response());
                    }
                }
                None => {
                    debug!("no Proxy-Authorization header, returning 407");
                    return Ok(proxy_auth_required_response());
                }
            }
        }

        // HTTP/2 CONNECT: hyper routes H2 CONNECT through the same Method::CONNECT
        // path but does not expose per-stream raw byte access via the upgrade
        // mechanism. Extended CONNECT tunneling (RFC 8441) is not supported;
        // return 501 so the client can retry over HTTP/1.1.
        if request.version() == Version::HTTP_2 {
            warn!("HTTP/2 CONNECT tunnel is not supported; client should use HTTP/1.1");
            return Ok(Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(crate::full_body(
                    "HTTP/2 CONNECT tunneling is not supported; use HTTP/1.1",
                ))?);
        }

        // Extract target authority from URI (e.g. "example.com:443").
        let authority = request
            .uri()
            .authority()
            .map(|a| a.to_string())
            .or_else(|| {
                request.uri().host().map(|h| {
                    let port = request.uri().port_u16().unwrap_or(443);
                    format!("{h}:{port}")
                })
            })
            .ok_or_else(|| ProxyError::BadRequest("CONNECT request missing authority".into()))?;

        debug!(target = %authority, "CONNECT tunnel request");

        // Connect to the target before responding to the client.
        let upstream = TcpStream::connect(&authority)
            .await
            .map_err(|e| ProxyError::Internal(format!("failed to connect to {authority}: {e}")))?;
        upstream.set_nodelay(true).ok();

        // Capture the upgrade future before consuming the request.
        // hyper stores the upgrade future in the request extensions.
        let client_upgrade = hyper::upgrade::on(&mut request);

        // Send 200 to signal that the tunnel is established.
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(empty_body())?;

        // Spawn the bidirectional copy. It runs once the client side
        // completes the upgrade (i.e. after the 200 is sent).
        tokio::spawn(async move {
            match client_upgrade.await {
                Ok(client_io) => {
                    let mut client_io = hyper_util::rt::TokioIo::new(client_io);
                    let mut upstream = upstream;

                    match copy_bidirectional(&mut client_io, &mut upstream).await {
                        Ok((up, down)) => {
                            debug!(
                                bytes_up = up,
                                bytes_down = down,
                                target = %authority,
                                "CONNECT tunnel closed"
                            );
                        }
                        Err(e) => {
                            debug!(error = %e, target = %authority, "CONNECT tunnel error");
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "CONNECT upgrade failed");
                }
            }
        });

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Proxy authentication helpers
// ---------------------------------------------------------------------------

/// Extract (username, password) from a `Proxy-Authorization: Basic <base64>`
/// header.
fn extract_proxy_credentials(req: &http::Request<Body>) -> Option<(String, String)> {
    let header_value = req.headers().get("proxy-authorization")?.to_str().ok()?;
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded_bytes = base64_decode(encoded)?;
    let decoded = String::from_utf8(decoded_bytes).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Minimal base64 decoder (mirrors the one in the auth middleware to avoid
/// an extra dependency).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let input = input.trim();
    if input.is_empty() {
        return Some(Vec::new());
    }

    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let val = match TABLE.iter().position(|&c| c == b) {
            Some(v) => v as u32,
            None => {
                if b == b'\n' || b == b'\r' || b == b' ' {
                    continue;
                }
                return None;
            }
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Some(output)
}

/// Verify a proxy auth user's password against their stored hash/plaintext.
fn verify_proxy_user(user: &ProxyAuthUser, username: &str, password: &str) -> bool {
    if user.username != username {
        return false;
    }
    if user.is_bcrypt {
        #[cfg(feature = "bcrypt")]
        {
            bcrypt::verify(password, &user.password_hash).unwrap_or(false)
        }
        #[cfg(not(feature = "bcrypt"))]
        {
            warn!("bcrypt password hash found but bcrypt feature is not enabled, rejecting");
            false
        }
    } else {
        constant_time_eq(password.as_bytes(), user.password_hash.as_bytes())
    }
}

/// Byte-level constant-time comparison to avoid timing side-channels.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build a 407 Proxy Authentication Required response with
/// `Proxy-Authenticate: Basic realm="gatel"`.
fn proxy_auth_required_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
        .header("Proxy-Authenticate", "Basic realm=\"gatel\"")
        .body(full_body("Proxy Authentication Required"))
        .unwrap()
}
