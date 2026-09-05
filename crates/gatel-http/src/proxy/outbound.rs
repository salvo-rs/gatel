//! Building the request that goes out, and writing the answer that comes back.
//!
//! Both directions rebuild the header map rather than editing it, so a
//! hop-by-hop header cannot survive by being spelled unusually, and neither
//! direction trusts anything the previous hop wrote about who the client is.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use gatel_state::{Body, BoxError, InFlight};
use http_body_util::{BodyExt, Full};
use salvo_core::http::{Request, Response};

use super::{Payload, ProxyError, ProxyTerminal};

/// Headers that describe one hop and must never be forwarded to the next one.
pub(super) const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

impl ProxyTerminal {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_request(
        &self,
        request: &mut Request,
        target: &str,
        host: &str,
        client_ip: Option<std::net::IpAddr>,
        tls: bool,
        upgrade_to: Option<&str>,
        payload: &Payload,
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
        // `Connection` and `Upgrade` are hop-by-hop and were stripped above.
        // For an upgrade they are the point of the request, so they go back —
        // narrowed to exactly the protocol that was asked for.
        if let Some(protocol) = upgrade_to
            && let Ok(value) = http::HeaderValue::from_str(protocol)
        {
            headers.insert(http::header::UPGRADE, value);
            headers.insert(
                http::header::CONNECTION,
                http::HeaderValue::from_static("upgrade"),
            );
        }

        // Whatever this attempt is allowed to send. A buffered body is cloned
        // rather than moved, because the next attempt may need it too.
        let body = match payload {
            Payload::Empty => Full::new(Bytes::new()).map_err(|never| match never {}).boxed(),
            Payload::Buffered(bytes) => Full::new(bytes.clone()).map_err(|never| match never {}).boxed(),
            Payload::Streaming => request
                .take_body()
                .map_err(|error| Box::new(error) as BoxError)
                .boxed(),
        };
        builder
            .body(body)
            .map_err(|error| ProxyError::Upstream(error.to_string()))
    }

    pub(super) fn write_response(
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

pub(super) fn cookie_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    let header = request.headers().get(http::header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value.trim())
    })
}

pub(super) fn connection_tokens(request: &Request) -> Vec<String> {
    request
        .headers()
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|token| token.trim().to_ascii_lowercase()))
        .collect()
}

/// Whether the request carries a body the upstream would have to read.
pub(super) fn has_body(request: &Request) -> bool {
    if request.headers().contains_key(http::header::TRANSFER_ENCODING) {
        return true;
    }
    declared_length(request).is_some_and(|length| length > 0)
}

/// How long the client said its body is, if it said.
///
/// `None` means chunked, or no body at all — the two cases the caller has to
/// tell apart with [`has_body`], and the two that cannot be checked against a
/// buffer budget before reading them.
pub(super) fn declared_length(request: &Request) -> Option<u64> {
    if request.headers().contains_key(http::header::TRANSFER_ENCODING) {
        return None;
    }
    request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
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
