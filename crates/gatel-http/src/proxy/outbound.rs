//! Building the request that goes out, and writing the answer that comes back.
//!
//! Both directions rebuild the header map rather than editing it, so a
//! hop-by-hop header cannot survive by being spelled unusually, and neither
//! direction trusts anything the previous hop wrote about who the client is.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use gatel_state::{Body, BoxError, InFlight, Lease};
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
            // HeaderName already stores its normalized lowercase spelling.
            let lower = name.as_str();
            if HOP_BY_HOP.contains(&lower) || connection_tokens.iter().any(|token| token == lower) {
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

    /// Hand the backend's answer to the client.
    ///
    /// `lease` is consumed rather than dropped by the caller, and that is the
    /// whole point: the body below is still being read off that connection,
    /// so the connection is not idle and must not go back into the pool until
    /// the body says it is finished.
    pub(super) fn write_response(
        &self,
        response: &mut Response,
        upstream: http::Response<hyper::body::Incoming>,
        in_flight: InFlight,
        lease: Lease,
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
            let lower = name.as_str();
            if HOP_BY_HOP.contains(&lower) || connection_tokens.iter().any(|token| token == lower) {
                continue;
            }
            response.headers_mut().append(name.clone(), value.clone());
        }
        // Both guards ride along with the body, so they are released when the
        // response finishes or is dropped rather than when this function
        // returns: the in-flight count, and the connection itself.
        let gap = self.upstream.spec.timeouts.body_idle;
        let guarded = GuardedBody {
            inner: body,
            idle: tokio::time::sleep(gap),
            gap,
            _guard: in_flight,
            _lease: lease,
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
    use http_body::Body as _;
    if request.headers().contains_key(http::header::TRANSFER_ENCODING) {
        return true;
    }
    declared_length(request).map_or_else(|| !request.body().is_end_stream(), |length| length > 0)
}

/// How long the client said its body is, if it said.
///
/// `None` means unknown length, including HTTP/2 DATA without Content-Length,
/// or no body at all. The caller distinguishes these using [`has_body`]; an
/// unknown length cannot be checked against a replay buffer budget in advance.
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
    /// A response body that keeps an in-flight guard alive, and gives up on a
    /// backend that stops sending it.
    ///
    /// `response-head` says how long the answer may take to start. Nothing said
    /// how long it may take to continue, so a backend that sent a header and
    /// then stalled held a connection, a pool slot and an in-flight count for
    /// as long as it liked — bounded only by `overall`, which is measured in
    /// minutes because it has to cover a legitimately long download.
    ///
    /// It also owns the lease, which is what keeps the connection out of the
    /// pool while the body is still coming down it.
    struct GuardedBody {
        #[pin]
        inner: hyper::body::Incoming,
        #[pin]
        idle: tokio::time::Sleep,
        // Reset after every frame, so the bound is on the gap between chunks
        // rather than on the length of the body.
        gap: Duration,
        _guard: InFlight,
        // The connection this body is arriving on. Held until the body is
        // done, because a connection that is still delivering a response is
        // not idle — parking it here let the next request be handed a socket
        // that was mid-answer, and hyper cancelled one of the two.
        _lease: Lease,
    }
}

impl http_body::Body for GuardedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.poll_frame(context) {
            Poll::Ready(frame) => {
                if frame.is_some() {
                    let gap = *this.gap;
                    this.idle.as_mut().reset(tokio::time::Instant::now() + gap);
                }
                Poll::Ready(frame.map(|result| result.map_err(|error| Box::new(error) as BoxError)))
            }
            // Nothing arrived. The response head is already on its way to the
            // client, so there is no status left to change: the only thing this
            // can do is stop, which truncates the body and frees everything the
            // stalled backend was holding.
            Poll::Pending => match this.idle.poll(context) {
                Poll::Ready(()) => Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("the backend stopped sending the response body for {:?}", this.gap),
                )) as BoxError))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
