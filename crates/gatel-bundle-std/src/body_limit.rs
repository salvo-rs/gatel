//! `body-limit`: what this route will read, when that differs from the rest.
//!
//! `global { limits body= }` already bounds every request the process reads;
//! this narrows or widens it for one site or route. The usual shape is a
//! generous global limit and a small one on everything that is not an upload
//! endpoint, so a route that never expected a body cannot be made to buffer
//! one.
//!
//! Two cases, because a client can declare a length, lie about it, or say
//! nothing at all. A declared length over the limit is refused before a byte is
//! read. A body with no declared length — chunked, or HTTP/2 — is counted as it
//! arrives and cut off the moment it goes past, which is the first point at
//! which there is anything to refuse.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{ReqBody, Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(arg)]
    max: ByteSize,
}

#[derive(Debug)]
pub struct Config {
    max: u64,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max);
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(BodyLimitHandler { max: self.max }))
    }

    fn describe(&self) -> String {
        format!("body-limit {}", ByteSize(self.max))
    }
}

pub struct BodyLimit;

impl Directive for BodyLimit {
    fn meta(&self) -> &DirectiveMeta {
        // A guard: it can refuse the request outright, and it must be in place
        // before anything downstream starts reading.
        static META: DirectiveMeta = DirectiveMeta::middleware("body-limit", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.max.bytes() == 0 {
            // Zero would be a route that accepts no body at all, which is a
            // real thing to want but not one to arrive at by typing "0".
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "a limit of zero refuses every request that has a body",
            );
            return None;
        }
        Some(Arc::new(Config {
            max: fields.max.bytes(),
        }))
    }
}

struct BodyLimitHandler {
    max: u64,
}

#[async_trait]
impl Handler for BodyLimitHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if let Some(declared) = declared_length(request)
            && declared > self.max
        {
            crate::generated(depot);
            response.status_code(StatusCode::PAYLOAD_TOO_LARGE);
            let _ = response.write_body(format!(
                "413 request body of {declared} bytes is larger than the {} byte limit\n",
                self.max
            ));
            ctrl.skip_rest();
            return;
        }

        let body = std::mem::take(request.body_mut());
        *request.body_mut() = ReqBody::Boxed {
            inner: Box::pin(Limited {
                inner: body,
                max: self.max,
                seen: 0,
            }),
            fuse_config: None,
        };
        ctrl.call_next(request, depot, response).await;
    }
}

fn declared_length(request: &Request) -> Option<u64> {
    request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

pin_project_lite::pin_project! {
    struct Limited {
        #[pin]
        inner: ReqBody,
        max: u64,
        seen: u64,
    }
}

impl http_body::Body for Limited {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    *this.seen += data.len() as u64;
                    if *this.seen > *this.max {
                        // The frame is not passed on: whatever is reading —
                        // a proxy writing to a backend, a form parser — sees
                        // the error rather than the excess.
                        return Poll::Ready(Some(Err(format!(
                            "request body is larger than the {} byte limit",
                            *this.max
                        )
                        .into())));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
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
    use gatel_model::{Origin, Value};
    use http::header::HeaderValue;
    use http_body::Body as _;

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(max: &str) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let node = Node::new("body-limit", origin()).with_arg(Value::string(max, origin()));
        let mut diagnostics = Diagnostics::new();
        let config = BodyLimit.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn handler(max: &str) -> Arc<dyn Handler> {
        decode(max)
            .0
            .unwrap()
            .build(BuildContext {
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap()
    }

    fn request(body: &'static [u8], declared: Option<&str>) -> Request {
        let mut request = Request::default();
        if let Some(declared) = declared {
            request.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                HeaderValue::from_str(declared).unwrap(),
            );
        }
        *request.body_mut() = ReqBody::Once(Bytes::from_static(body));
        request
    }

    async fn run(handler: &dyn Handler, request: &mut Request) -> Response {
        let mut response = Response::new();
        handler
            .handle(
                request,
                &mut Depot::new(),
                &mut response,
                &mut FlowCtrl::new(Vec::new()),
            )
            .await;
        response
    }

    async fn drain(request: &mut Request) -> Result<u64, String> {
        let mut body = std::mem::take(request.body_mut());
        let mut total = 0;
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let frame = frame.map_err(|error| error.to_string())?;
            if let Some(data) = frame.data_ref() {
                total += data.len() as u64;
            }
        }
        Ok(total)
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_without_reading_it() {
        let handler = handler("64");
        // The body is never sent; only the claim about it.
        let mut request = request(b"", Some("5000"));
        let response = run(handler.as_ref(), &mut request).await;
        assert_eq!(response.status_code, Some(StatusCode::PAYLOAD_TOO_LARGE));
    }

    #[tokio::test]
    async fn a_declared_length_that_fits_is_passed_on() {
        let handler = handler("64");
        let mut request = request(b"hello", Some("5"));
        let response = run(handler.as_ref(), &mut request).await;
        assert_eq!(response.status_code, None);
        assert_eq!(drain(&mut request).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn an_undeclared_body_is_cut_off_when_it_passes_the_limit() {
        let handler = handler("4");
        let mut request = request(b"0123456789", None);
        run(handler.as_ref(), &mut request).await;
        let error = drain(&mut request).await.unwrap_err();
        assert!(error.contains("larger than the 4 byte limit"), "{error}");
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_limit_is_allowed() {
        let handler = handler("4");
        let mut request = request(b"0123", None);
        run(handler.as_ref(), &mut request).await;
        assert_eq!(drain(&mut request).await.unwrap(), 4);
    }

    #[test]
    fn a_limit_of_zero_is_refused() {
        let (config, diagnostics) = decode("0");
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("refuses every request"))
        );
    }

    #[test]
    fn a_size_is_written_the_way_it_was_meant() {
        assert_eq!(decode("8MiB").0.unwrap().describe(), "body-limit 8MiB");
    }

    #[test]
    fn it_is_a_guard_so_it_is_in_place_before_anything_reads() {
        assert_eq!(BodyLimit.meta().role, gatel_plan::Role::Middleware(Phase::Guard));
    }
}
