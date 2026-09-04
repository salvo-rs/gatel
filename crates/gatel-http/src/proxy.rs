//! The reverse proxy terminal.
//!
//! This lives in the kernel rather than in a bundle because endpoint
//! selection, health, connection reuse and retry are one correctness chain: a
//! plugin that could replace any single link could also break the others.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use gatel_state::{Body, BoxError, CheckoutError, Endpoint, InFlight, RequestKey, UpstreamRuntime, select};
use http_body_util::BodyExt;
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

use crate::dispatch::request_host;

/// Headers that describe one hop and must never be forwarded to the next one.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// A proxy terminal bound to one upstream's runtime.
pub struct ProxyTerminal {
    upstream: Arc<UpstreamRuntime>,
}

impl std::fmt::Debug for ProxyTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTerminal")
            .field("upstream", &self.upstream.spec.id.name())
            .finish()
    }
}

impl ProxyTerminal {
    pub fn new(upstream: Arc<UpstreamRuntime>) -> Self {
        Self { upstream }
    }
}

/// Why a proxied request failed, mapped to what the client is told.
#[derive(Debug)]
enum ProxyError {
    /// Nothing is healthy, so there is nowhere to send it.
    NoEndpoint,
    /// A connection could not be obtained.
    Checkout(CheckoutError),
    /// The upstream did not answer in time.
    Timeout(&'static str, Duration),
    /// The upstream answered badly, or not at all.
    Upstream(String),
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            ProxyError::NoEndpoint => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::Checkout(CheckoutError::QueueFull { .. } | CheckoutError::QueueTimeout(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProxyError::Checkout(_) => StatusCode::BAD_GATEWAY,
            ProxyError::Timeout(..) => StatusCode::GATEWAY_TIMEOUT,
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// What the client sees. Never the upstream's address or error text: that
    /// belongs in the log, not in a response to whoever asked.
    fn public_message(&self) -> &'static str {
        match self {
            ProxyError::NoEndpoint => "503 no healthy backend\n",
            ProxyError::Checkout(CheckoutError::QueueFull { .. } | CheckoutError::QueueTimeout(_)) => {
                "503 backend at capacity\n"
            }
            ProxyError::Checkout(_) => "502 could not reach the backend\n",
            ProxyError::Timeout(..) => "504 the backend did not answer in time\n",
            ProxyError::Upstream(_) => "502 the backend replied badly\n",
        }
    }

    fn detail(&self) -> String {
        match self {
            ProxyError::NoEndpoint => "no eligible endpoint".to_string(),
            ProxyError::Checkout(error) => error.summary(),
            ProxyError::Timeout(stage, after) => format!("{stage} exceeded {after:?}"),
            ProxyError::Upstream(message) => message.clone(),
        }
    }
}

#[async_trait]
impl Handler for ProxyTerminal {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let started = std::time::Instant::now();
        match self.proxy(request, depot, response).await {
            Ok(attempts) => {
                depot.insert("gatel.upstream_attempts", attempts);
            }
            Err(error) => {
                tracing::warn!(
                    target: "gatel::upstream",
                    event = "upstream_failed",
                    upstream = self.upstream.spec.id.name(),
                    status = error.status().as_u16(),
                    detail = error.detail(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                );
                response.status_code(error.status());
                let _ = response.write_body(error.public_message());
            }
        }
    }
}

impl ProxyTerminal {
    async fn proxy(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
    ) -> Result<u32, ProxyError> {
        let spec = &self.upstream.spec;
        let timeouts = &spec.timeouts;
        let deadline = tokio::time::Instant::now() + timeouts.overall;

        let client_ip = depot.get::<std::net::IpAddr>("gatel.client_ip").ok().copied();
        let tls = depot.get::<bool>("gatel.tls").ok().copied().unwrap_or(false);
        let uri = request.uri().clone();
        let target = uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .to_string();
        let host = request_host(request);

        // Only a request whose body is known to be absent may be retried on
        // another endpoint, and only when nothing was sent yet.
        let retriable_body = !has_body(request);
        let max_attempts = match &spec.retry {
            Some(retry) if retriable_body => retry.attempts.max(1),
            _ => 1,
        };

        let key = RequestKey {
            client_ip,
            uri: &target,
            header: match &spec.balance {
                gatel_plan::Balance::HeaderHash { header } => request
                    .headers()
                    .get(header.as_str())
                    .and_then(|value| value.to_str().ok()),
                _ => None,
            },
            cookie: match &spec.balance {
                gatel_plan::Balance::CookieHash { cookie } => cookie_value(request, cookie),
                _ => None,
            },
        };

        let mut excluded: Vec<String> = Vec::new();
        let mut last: Option<ProxyError> = None;

        for attempt in 1..=max_attempts {
            let eligible: Vec<Arc<Endpoint>> = self
                .upstream
                .endpoints
                .eligible()
                .into_iter()
                .filter(|endpoint| !excluded.contains(&endpoint.id))
                .collect();

            let Some(selection) = select(&spec.balance, &eligible, &self.upstream.selector, &key) else {
                return Err(last.unwrap_or(ProxyError::NoEndpoint));
            };
            let endpoint = Arc::clone(&selection.endpoint);
            // Held until the response body finishes, so `least_conn` counts
            // work rather than sockets.
            let in_flight = self.upstream.endpoints.claim(&endpoint);

            let checkout =
                tokio::time::timeout_at(deadline, self.upstream.pool.checkout(&endpoint, timeouts)).await;

            let mut lease = match checkout {
                Ok(Ok(lease)) => lease,
                Ok(Err(error)) => {
                    // Every checkout failure happened before a byte was sent,
                    // so trying elsewhere cannot duplicate anything.
                    tracing::debug!(
                        target: "gatel::upstream",
                        event = "checkout_failed",
                        endpoint = %endpoint.id,
                        attempt,
                        detail = error.summary(),
                    );
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    excluded.push(endpoint.id.clone());
                    last = Some(ProxyError::Checkout(error));
                    continue;
                }
                Err(_) => return Err(ProxyError::Timeout("overall", timeouts.overall)),
            };

            let outbound = self.build_request(request, &target, &host, client_ip, tls)?;
            let sent = tokio::time::timeout(timeouts.response_head, lease.send(outbound)).await;

            let upstream_response = match sent {
                Ok(Ok(upstream_response)) => upstream_response,
                Ok(Err(error)) => {
                    // The request was already on the wire: the upstream may
                    // have acted on it, so this is never retried.
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    return Err(ProxyError::Upstream(error.to_string()));
                }
                Err(_) => {
                    self.upstream.endpoints.report_traffic(&endpoint.id, true);
                    return Err(ProxyError::Timeout("response-head", timeouts.response_head));
                }
            };

            let status = upstream_response.status().as_u16();
            let failed = self.upstream.endpoints.counts_as_failure(status);
            if self.upstream.endpoints.report_traffic(&endpoint.id, failed) {
                tracing::info!(
                    target: "gatel::upstream",
                    event = "endpoint_ejected",
                    upstream = self.upstream.spec.id.name(),
                    endpoint = %endpoint.id,
                    status,
                );
            }

            self.write_response(response, upstream_response, in_flight);
            return Ok(attempt);
        }

        Err(last.unwrap_or(ProxyError::NoEndpoint))
    }

    fn build_request(
        &self,
        request: &mut Request,
        target: &str,
        host: &str,
        client_ip: Option<std::net::IpAddr>,
        tls: bool,
    ) -> Result<http::Request<Body>, ProxyError> {
        let mut builder = http::Request::builder()
            .method(request.method().clone())
            .uri(target);

        let headers = builder.headers_mut().expect("a fresh builder has no error");
        // Rebuild the header map rather than editing it, so a hop-by-hop
        // header cannot survive by being spelled unusually.
        let connection_tokens = connection_tokens(request);
        for (name, value) in request.headers() {
            let lower = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) || connection_tokens.contains(&lower) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_str(host)
                .map_err(|_| ProxyError::Upstream("the request host is not a valid header value".into()))?,
        );
        // Forwarding headers are rewritten rather than appended to: the
        // dispatcher already decided which address is believable, and passing
        // an untrusted chain along would let a client choose its own identity
        // at the next hop.
        if let Some(ip) = client_ip
            && let Ok(value) = http::HeaderValue::from_str(&ip.to_string())
        {
            headers.insert("x-forwarded-for", value);
        }
        if let Ok(value) = http::HeaderValue::from_str(host) {
            headers.insert("x-forwarded-host", value);
        }
        headers.insert(
            "x-forwarded-proto",
            http::HeaderValue::from_static(if tls { "https" } else { "http" }),
        );

        let body = request
            .take_body()
            .map_err(|error| Box::new(error) as BoxError)
            .boxed();
        builder
            .body(body)
            .map_err(|error| ProxyError::Upstream(error.to_string()))
    }

    fn write_response(
        &self,
        response: &mut Response,
        upstream: http::Response<hyper::body::Incoming>,
        in_flight: InFlight,
    ) {
        let (parts, body) = upstream.into_parts();
        response.status_code(parts.status);
        let connection_tokens: Vec<String> = parts
            .headers
            .get_all(http::header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(',').map(|token| token.trim().to_ascii_lowercase()))
            .collect();
        for (name, value) in &parts.headers {
            let lower = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) || connection_tokens.contains(&lower) {
                continue;
            }
            response.headers_mut().append(name.clone(), value.clone());
        }
        // The guard rides along with the body so the in-flight count falls when
        // the response finishes or is dropped, not when this function returns.
        let guarded = GuardedBody {
            inner: body,
            _guard: in_flight,
        };
        response.replace_body(salvo_core::http::ResBody::Boxed(Box::pin(guarded)));
    }
}

fn cookie_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    let header = request.headers().get(http::header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value.trim())
    })
}

fn connection_tokens(request: &Request) -> Vec<String> {
    request
        .headers()
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|token| token.trim().to_ascii_lowercase()))
        .collect()
}

/// Whether the request carries a body the upstream would have to read.
fn has_body(request: &Request) -> bool {
    if request.headers().contains_key(http::header::TRANSFER_ENCODING) {
        return true;
    }
    request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > 0)
}

pin_project_lite::pin_project! {
    /// A response body that keeps an in-flight guard alive.
    struct GuardedBody {
        #[pin]
        inner: hyper::body::Incoming,
        _guard: InFlight,
    }
}

impl http_body::Body for GuardedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.project()
            .inner
            .poll_frame(context)
            .map(|frame| frame.map(|result| result.map_err(|error| Box::new(error) as BoxError)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use salvo_core::http::header::HeaderValue;

    use super::*;

    fn request_with(headers: &[(&str, &str)]) -> Request {
        let mut request = Request::default();
        for (name, value) in headers {
            request.headers_mut().append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        request
    }

    #[test]
    fn a_request_without_a_body_may_be_retried() {
        assert!(!has_body(&request_with(&[])));
        assert!(!has_body(&request_with(&[("content-length", "0")])));
        assert!(has_body(&request_with(&[("content-length", "12")])));
        assert!(has_body(&request_with(&[("transfer-encoding", "chunked")])));
    }

    #[test]
    fn headers_named_by_connection_are_treated_as_hop_by_hop() {
        let request = request_with(&[("connection", "keep-alive, X-Custom-Hop"), ("x-custom-hop", "1")]);
        let tokens = connection_tokens(&request);
        assert!(tokens.contains(&"x-custom-hop".to_string()));
        assert!(tokens.contains(&"keep-alive".to_string()));
    }

    #[test]
    fn cookies_are_found_among_others() {
        let request = request_with(&[("cookie", "a=1; session=abc; b=2")]);
        assert_eq!(cookie_value(&request, "session"), Some("abc"));
        assert_eq!(cookie_value(&request, "absent"), None);
    }

    #[test]
    fn failures_never_leak_the_backend_into_the_response() {
        let error = ProxyError::Upstream("connection refused by 10.0.0.7:9000".into());
        assert!(!error.public_message().contains("10.0.0.7"));
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        // The detail is still available for the log.
        assert!(error.detail().contains("10.0.0.7"));
    }

    #[test]
    fn capacity_problems_are_not_reported_as_bad_gateway() {
        let full = ProxyError::Checkout(CheckoutError::QueueFull { waiting: 9, limit: 8 });
        assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
        let refused = ProxyError::Checkout(CheckoutError::Connect("refused".into()));
        assert_eq!(refused.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            ProxyError::Timeout("response-head", Duration::from_secs(1)).status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }
}
