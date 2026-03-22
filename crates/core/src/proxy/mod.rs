pub mod cgi;
pub mod circuit_breaker;
pub mod dns_upstream;
pub mod fastcgi;
pub mod forward_proxy;
pub mod health;
pub mod lb;
pub mod scgi;
pub mod srv_upstream;
#[cfg(unix)]
pub mod unix_upstream;
pub mod upstream;
pub mod websocket;

use std::collections::HashMap;
use std::sync::Arc;

use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use regex::Regex;
use tracing::{debug, warn};

use self::health::{HealthChecker, PassiveHealthChecker};
use self::lb::*;
use self::upstream::UpstreamPool;
use crate::config::{LbPolicy, ProxyConfig};
use crate::{Body, ProxyError, goals};

/// Reverse proxy handler: forwards requests to upstream backends with
/// load balancing, health checking, and retry support.
pub struct ReverseProxy {
    pool: Arc<UpstreamPool>,
    lb: Box<dyn LoadBalancer>,
    headers_up: Vec<(String, String)>,
    headers_down: Vec<(String, String)>,
    retries: u32,
    /// Active health checker (holds the background task; dropped with the proxy).
    _health_checker: Option<HealthChecker>,
    /// Passive health checker (tracks 5xx responses).
    passive_health: Option<Arc<PassiveHealthChecker>>,
    /// Custom response bodies substituted when upstream returns these status codes.
    error_pages: HashMap<u16, String>,
    /// Compiled regex replacement rules for upstream request headers.
    /// Each entry is `(header_name, compiled_regex, replacement)`.
    headers_up_replace: Vec<(String, Regex, String)>,
    /// When true, normalize the URI path before forwarding (collapse double
    /// slashes, resolve `.` and `..` segments).
    sanitize_uri: bool,
}

impl ReverseProxy {
    pub fn new(config: &ProxyConfig) -> Self {
        let pool = Arc::new(UpstreamPool::from_config(config));

        // Collect weights for weighted strategies.
        let weights: Vec<u32> = config.upstreams.iter().map(|u| u.weight).collect();

        let lb: Box<dyn LoadBalancer> = match config.lb {
            LbPolicy::RoundRobin => Box::new(RoundRobinLb::new()),
            LbPolicy::Random => Box::new(RandomLb::new()),
            LbPolicy::WeightedRoundRobin => Box::new(WeightedRoundRobinLb::new(&weights)),
            LbPolicy::IpHash => Box::new(IpHashLb::new()),
            LbPolicy::LeastConn => Box::new(LeastConnLb::new()),
            LbPolicy::UriHash => Box::new(UriHashLb::new()),
            LbPolicy::HeaderHash => {
                let name = config
                    .lb_header
                    .clone()
                    .unwrap_or_else(|| "X-Forwarded-For".to_string());
                Box::new(HeaderHashLb::new(name))
            }
            LbPolicy::CookieHash => {
                let name = config
                    .lb_cookie
                    .clone()
                    .unwrap_or_else(|| "session".to_string());
                Box::new(CookieHashLb::new(name))
            }
            LbPolicy::First => Box::new(FirstLb::new()),
            LbPolicy::TwoRandomChoices => Box::new(TwoRandomChoicesLb::new()),
        };

        let headers_up: Vec<(String, String)> = config
            .headers_up
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let headers_down: Vec<(String, String)> = config
            .headers_down
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Start active health checker if configured.
        let health_checker = config
            .health_check
            .as_ref()
            .map(|hc| HealthChecker::start(Arc::clone(&pool), hc));

        // Create passive health checker if configured.
        let passive_health = config
            .passive_health
            .as_ref()
            .map(|ph| Arc::new(PassiveHealthChecker::new(pool.len(), ph)));

        // Compile regex replacement rules for upstream headers.
        let headers_up_replace: Vec<(String, Regex, String)> = config
            .headers_up_replace
            .iter()
            .filter_map(|(name, pattern, replacement)| match Regex::new(pattern) {
                Ok(re) => Some((name.clone(), re, replacement.clone())),
                Err(e) => {
                    warn!(
                        header = name.as_str(),
                        pattern = pattern.as_str(),
                        error = %e,
                        "invalid regex in header-up-replace, skipping"
                    );
                    None
                }
            })
            .collect();

        Self {
            pool,
            lb,
            headers_up,
            headers_down,
            retries: config.retries,
            _health_checker: health_checker,
            passive_health,
            error_pages: config.error_pages.clone(),
            headers_up_replace,
            sanitize_uri: config.sanitize_uri,
        }
    }
}

#[salvo::async_trait]
impl salvo::Handler for ReverseProxy {
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
            .proxy(request, client_addr)
            .await
            .unwrap_or_else(|e| e.into_response());
        goals::merge_response(res, response);
        ctrl.skip_rest();
    }
}

impl ReverseProxy {
    async fn proxy(
        &self,
        request: Request<Body>,
        client_addr: std::net::SocketAddr,
    ) -> Result<Response<Body>, ProxyError> {
        // --- WebSocket upgrade detection ---
        // If the request is a WebSocket upgrade, use the dedicated
        // WebSocket proxy path instead of the normal HTTP proxy.
        if websocket::is_websocket_upgrade(&request) {
            debug!(client = %client_addr, "detected WebSocket upgrade request");

            // Select a backend for the WebSocket connection.
            let ws_lb_ctx = LbContext {
                client_addr,
                uri: request
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string()),
                headers: request.headers().clone(),
            };

            let backend_idx = self
                .lb
                .select(&self.pool, &ws_lb_ctx)
                .ok_or(ProxyError::NoUpstream)?;
            let backend = &self.pool.backends[backend_idx];
            let _conn_guard = self.pool.acquire_conn(backend_idx);

            return websocket::proxy_websocket(request, &backend.addr).await;
        }

        // Build LbContext from the incoming request.
        let lb_ctx = LbContext {
            client_addr,
            uri: request
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string()),
            headers: request.headers().clone(),
        };

        // --- Connection limit check ---
        if let Some(max_conns) = self.pool.max_connections {
            let total = self.pool.total_active_conns();
            if total >= max_conns {
                warn!(
                    limit = max_conns,
                    active = total,
                    "connection limit exceeded, returning 503"
                );
                return Response::builder()
                    .status(http::StatusCode::SERVICE_UNAVAILABLE)
                    .body(crate::full_body(
                        "Service Unavailable: connection limit exceeded",
                    ))
                    .map_err(|e| ProxyError::Internal(e.to_string()));
            }
        }

        // Collect the body bytes so we can replay on retries.
        let (parts, body) = request.into_parts();
        let body_bytes = body
            .collect()
            .await
            .map_err(|e| ProxyError::Internal(format!("failed to buffer body: {e}")))?
            .to_bytes();

        let max_attempts = 1 + self.retries;
        let mut last_failed_idx: Option<usize> = None;
        let mut last_error: Option<ProxyError> = None;

        for attempt in 0..max_attempts {
            // --- Select a backend ---
            let backend_idx = {
                let idx = self.lb.select(&self.pool, &lb_ctx);
                match idx {
                    Some(i) if last_failed_idx == Some(i) && self.pool.len() > 1 => {
                        // On retry, try to skip the backend that just failed.
                        self.lb.select(&self.pool, &lb_ctx)
                    }
                    other => other,
                }
            };

            let backend_idx = match backend_idx {
                Some(i) => i,
                None => {
                    return Err(last_error.unwrap_or(ProxyError::NoUpstream));
                }
            };

            let backend = &self.pool.backends[backend_idx];

            // --- Build the upstream request ---
            let mut req_parts = parts.clone();

            // Optionally sanitize the URI path before forwarding.
            if self.sanitize_uri {
                let raw_pq = req_parts
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string());
                let (raw_path, raw_query) = if let Some(pos) = raw_pq.find('?') {
                    (&raw_pq[..pos], Some(&raw_pq[pos + 1..]))
                } else {
                    (raw_pq.as_str(), None)
                };
                let sanitized_path = sanitize_path(raw_path);
                let sanitized_pq = match raw_query {
                    Some(q) if !q.is_empty() => format!("{sanitized_path}?{q}"),
                    _ => sanitized_path,
                };
                if let Ok(new_uri) = sanitized_pq.parse::<http::uri::PathAndQuery>() {
                    // Rebuild the URI with sanitized path-and-query.
                    let mut builder = Uri::builder();
                    if let Some(scheme) = req_parts.uri.scheme() {
                        builder = builder.scheme(scheme.clone());
                    }
                    if let Some(authority) = req_parts.uri.authority() {
                        builder = builder.authority(authority.clone());
                    }
                    builder = builder.path_and_query(new_uri);
                    if let Ok(u) = builder.build() {
                        req_parts.uri = u;
                    }
                }
            }

            // Use the scheme embedded in the backend address if present,
            // otherwise default to "http://".
            let scheme =
                if backend.addr.starts_with("https://") || backend.addr.starts_with("http://") {
                    ""
                } else {
                    "http://"
                };
            let upstream_uri = format!(
                "{}{}{}",
                scheme,
                backend.addr,
                req_parts
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or("/")
            );
            req_parts.uri = match upstream_uri.parse::<Uri>() {
                Ok(u) => u,
                Err(e) => {
                    return Err(ProxyError::Internal(format!("invalid upstream URI: {e}")));
                }
            };

            // Set the Host header to the upstream.
            if let Ok(hv) = backend.addr.parse() {
                req_parts.headers.insert(http::header::HOST, hv);
            }

            // Apply header-up directives.
            for (name, value) in &self.headers_up {
                if let Some(hdr_name) = name.strip_prefix('-') {
                    if let Ok(hn) = hdr_name.parse::<http::header::HeaderName>() {
                        req_parts.headers.remove(hn);
                    }
                } else {
                    let expanded = value.replace("{client_ip}", &client_addr.ip().to_string());
                    if let (Ok(hn), Ok(hv)) = (
                        name.parse::<http::header::HeaderName>(),
                        expanded.parse::<http::header::HeaderValue>(),
                    ) {
                        req_parts.headers.insert(hn, hv);
                    }
                }
            }

            // Apply header-up-replace directives (regex substitution on existing values).
            for (name, re, replacement) in &self.headers_up_replace {
                if let Ok(hn) = name.parse::<http::header::HeaderName>()
                    && let Some(existing) = req_parts.headers.get(&hn)
                    && let Ok(existing_str) = existing.to_str()
                {
                    let new_value = re.replace_all(existing_str, replacement.as_str());
                    if let Ok(hv) = new_value.as_ref().parse::<http::header::HeaderValue>() {
                        req_parts.headers.insert(hn, hv);
                    }
                }
            }

            // Replay body from the buffered bytes.
            let req_body = crate::full_body(body_bytes.clone());
            let upstream_req = Request::from_parts(req_parts, req_body);

            debug!(
                upstream = %backend.addr,
                attempt = attempt + 1,
                "forwarding request"
            );

            // --- Track active connections ---
            let _conn_guard = self.pool.acquire_conn(backend_idx);

            // --- Send the request ---
            // Unix socket backends bypass the shared HTTPS connector.
            let result = if is_unix_socket(&backend.addr) {
                #[cfg(unix)]
                {
                    let path = unix_socket_path(&backend.addr);
                    send_via_unix(path, upstream_req).await.map(|r| {
                        r.map(|b| {
                            let b: Body = b.map_err(|e| -> crate::BoxError { Box::new(e) }).boxed();
                            b
                        })
                    })
                }
                #[cfg(not(unix))]
                {
                    let _ = upstream_req;
                    Err(ProxyError::Internal(
                        "Unix domain socket upstreams are not supported on this platform".into(),
                    ))
                }
            } else {
                self.pool
                    .client
                    .request(upstream_req)
                    .await
                    .map_err(ProxyError::Client)
                    .map(|r| {
                        r.map(|b| {
                            let b: Body = b.map_err(|e| -> crate::BoxError { Box::new(e) }).boxed();
                            b
                        })
                    })
            };

            match result {
                Ok(resp) => {
                    let (mut resp_parts, resp_body) = resp.into_parts();

                    // Passive health: record 5xx
                    if resp_parts.status.is_server_error()
                        && let Some(ref ph) = self.passive_health
                    {
                        ph.record_failure(backend_idx, &self.pool).await;
                    }

                    // If server error and we have retries left, retry.
                    if resp_parts.status.is_server_error() && attempt + 1 < max_attempts {
                        warn!(
                            upstream = %backend.addr,
                            status = %resp_parts.status,
                            attempt = attempt + 1,
                            "upstream returned server error, retrying"
                        );
                        last_failed_idx = Some(backend_idx);
                        last_error = Some(ProxyError::Internal(format!(
                            "upstream {} returned {}",
                            backend.addr, resp_parts.status
                        )));
                        continue;
                    }

                    // resp_body is already typed as Body (mapped above).

                    // Apply header-down directives.
                    for (name, value) in &self.headers_down {
                        if let Some(hdr_name) = name.strip_prefix('-') {
                            if let Ok(hn) = hdr_name.parse::<http::header::HeaderName>() {
                                resp_parts.headers.remove(hn);
                            }
                        } else if let (Ok(hn), Ok(hv)) = (
                            name.parse::<http::header::HeaderName>(),
                            value.parse::<http::header::HeaderValue>(),
                        ) {
                            resp_parts.headers.insert(hn, hv);
                        }
                    }

                    // Passive health recovery check.
                    if let Some(ref ph) = self.passive_health {
                        ph.maybe_recover(&self.pool).await;
                    }

                    // Error page interception: if the upstream status code has a
                    // configured error page, replace the response body with it.
                    let status_code = resp_parts.status.as_u16();
                    if let Some(error_body) = self.error_pages.get(&status_code) {
                        debug!(
                            status = status_code,
                            "intercepting upstream error with configured error page"
                        );
                        return Ok(Response::from_parts(
                            resp_parts,
                            crate::full_body(error_body.clone()),
                        ));
                    }

                    return Ok(Response::from_parts(resp_parts, resp_body));
                }
                Err(e) => {
                    // Passive health: record connection failure as well.
                    if let Some(ref ph) = self.passive_health {
                        ph.record_failure(backend_idx, &self.pool).await;
                    }

                    if attempt + 1 < max_attempts {
                        warn!(
                            upstream = %backend.addr,
                            error = %e,
                            attempt = attempt + 1,
                            "upstream request failed, retrying"
                        );
                        last_failed_idx = Some(backend_idx);
                        last_error = Some(e);
                        continue;
                    }

                    return Err(e);
                }
            }
        }

        // Should not be reached, but just in case:
        Err(last_error.unwrap_or(ProxyError::NoUpstream))
    }
}

// ---------------------------------------------------------------------------
// URI sanitization helpers
// ---------------------------------------------------------------------------

/// Sanitize a URI path by:
/// 1. Collapsing consecutive slashes (e.g. `//foo///bar` → `/foo/bar`).
/// 2. Resolving `.` (current-directory) segments.
/// 3. Resolving `..` (parent-directory) segments without escaping the root.
fn sanitize_path(path: &str) -> String {
    // Split on '/' and process segments.
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {
                // Skip empty segments (produced by consecutive slashes) and `.`
            }
            ".." => {
                // Go up one level, but never above root.
                segments.pop();
            }
            s => {
                segments.push(s);
            }
        }
    }
    let mut result = String::with_capacity(path.len());
    result.push('/');
    result.push_str(&segments.join("/"));
    result
}

// ---------------------------------------------------------------------------
// Unix socket upstream helpers
// ---------------------------------------------------------------------------

/// Returns true when the backend address is a Unix domain socket path.
/// Accepts paths starting with `"unix:"` (scheme prefix) or `"/"` (absolute).
fn is_unix_socket(addr: &str) -> bool {
    addr.starts_with("unix:") || addr.starts_with('/')
}

/// Strip the optional `"unix:"` scheme prefix from a socket path.
#[cfg(unix)]
fn unix_socket_path(addr: &str) -> &str {
    addr.strip_prefix("unix:").unwrap_or(addr)
}

/// Send an HTTP/1.1 request over a Unix domain socket and return the response.
///
/// This bypasses the shared HTTPS connector pool and instead opens a fresh
/// `UnixStream`, performs an HTTP/1 handshake, and sends the request.
#[cfg(unix)]
async fn send_via_unix(
    socket_path: &str,
    request: http::Request<Body>,
) -> Result<http::Response<hyper::body::Incoming>, ProxyError> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| ProxyError::Internal(format!("unix socket connect to {socket_path}: {e}")))?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(ProxyError::Hyper)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(ProxyError::Hyper)
}
