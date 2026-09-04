//! `access-log`: one event per request, when the request is actually over.
//!
//! A handler returning means the response head was decided, not that the body
//! reached the client. Logging there would report a streaming download as
//! instant and a cancelled one as successful. This directive therefore wraps
//! the response body and reports when that body ends, fails or is dropped.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, ResBody, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The body reached its end.
    Completed,
    /// The body failed part way through.
    Failed,
    /// The client went away, or the response was dropped, before the end.
    Cancelled,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "body_error",
            Outcome::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(prop, default = String::from("text"))]
    format: String,
    /// Off by default: a query string routinely carries tokens and personal
    /// data, and a log is the wrong place for either.
    #[gatel(prop, default = false)]
    query: bool,
}

#[derive(Debug)]
pub struct Config {
    json: bool,
    query: bool,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_bool(self.json);
        hasher.update_bool(self.query);
    }

    fn build(
        &self,
        _state: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(AccessLogHandler {
            json: self.json,
            query: self.query,
        }))
    }

    fn describe(&self) -> String {
        format!("access-log {}", if self.json { "json" } else { "text" })
    }
}

pub struct AccessLog;

impl Directive for AccessLog {
    fn meta(&self) -> &DirectiveMeta {
        // Outermost, so it observes what every other directive did — including
        // a guard that rejected the request before it reached a terminal. Fail
        // open: a gateway that stops serving because it cannot log is worse
        // than one that serves without a log line.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("access-log", Phase::Observe, Scopes::BOTH).fail_open();
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        let json = match fields.format.as_str() {
            "text" => false,
            "json" => true,
            other => {
                diagnostics.push(
                    gatel_model::Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("unknown access log format `{other}`"),
                    )
                    .with_help("known formats: text, json"),
                );
                return None;
            }
        };
        Some(Arc::new(Config {
            json,
            query: fields.query,
        }))
    }
}

struct AccessLogHandler {
    json: bool,
    query: bool,
}

#[async_trait]
impl Handler for AccessLogHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let started = Instant::now();
        let method = request.method().to_string();
        let path = if self.query {
            request.uri().path_and_query().map(|value| value.to_string())
        } else {
            Some(request.uri().path().to_string())
        }
        .unwrap_or_else(|| "/".to_string());

        ctrl.call_next(request, depot, response).await;

        let status = response.status_code.map(|code| code.as_u16()).unwrap_or(200);
        let route = depot.get::<String>("gatel.route").cloned().unwrap_or_default();
        let record = Record {
            json: self.json,
            method,
            path,
            status,
            route,
            started,
        };

        // Anything already fully in memory is finished now; a streaming body
        // is not, and gets a wrapper that reports when it really ends.
        match std::mem::take(response.body_mut()) {
            ResBody::None => {
                record.emit(Outcome::Completed, 0);
                *response.body_mut() = ResBody::None;
            }
            ResBody::Once(bytes) => {
                record.emit(Outcome::Completed, bytes.len() as u64);
                *response.body_mut() = ResBody::Once(bytes);
            }
            ResBody::Chunks(chunks) => {
                record.emit(
                    Outcome::Completed,
                    chunks.iter().map(|chunk| chunk.len() as u64).sum(),
                );
                *response.body_mut() = ResBody::Chunks(chunks);
            }
            other => {
                *response.body_mut() = ResBody::Boxed(Box::pin(ObservedBody {
                    inner: other,
                    record: Some(record),
                    sent: 0,
                }));
            }
        }
    }
}

struct Record {
    json: bool,
    method: String,
    path: String,
    status: u16,
    route: String,
    started: Instant,
}

impl Record {
    fn emit(&self, outcome: Outcome, bytes: u64) {
        let duration = self.started.elapsed().as_millis() as u64;
        if self.json {
            tracing::info!(
                target: "gatel::access",
                event = "request_completed",
                method = %self.method,
                path = %self.path,
                status = self.status,
                route = %self.route,
                duration_ms = duration,
                bytes,
                outcome = outcome.as_str(),
            );
        } else {
            tracing::info!(
                target: "gatel::access",
                "{} {} {} {}ms {}B {}",
                self.method,
                self.path,
                self.status,
                duration,
                bytes,
                outcome.as_str(),
            );
        }
    }
}

pin_project_lite::pin_project! {
    /// Wraps a response body and reports how it ended.
    ///
    /// `PinnedDrop` is what makes a cancelled request distinguishable from a
    /// completed one: if the body is dropped before its end, that is the only
    /// place anyone finds out.
    struct ObservedBody {
        #[pin]
        inner: ResBody,
        record: Option<Record>,
        sent: u64,
    }

    impl PinnedDrop for ObservedBody {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(record) = this.record.take() {
                record.emit(Outcome::Cancelled, *this.sent);
            }
        }
    }
}

impl http_body::Body for ObservedBody {
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
                    *this.sent += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(record) = this.record.take() {
                    record.emit(Outcome::Failed, *this.sent);
                }
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => {
                if let Some(record) = this.record.take() {
                    record.emit(Outcome::Completed, *this.sent);
                }
                Poll::Ready(None)
            }
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

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = AccessLog.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    #[test]
    fn the_default_is_text_without_the_query_string() {
        let (config, diagnostics) = decode(Node::new("access-log", origin()));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "access-log text");
    }

    #[test]
    fn an_unknown_format_lists_the_known_ones() {
        let node = Node::new("access-log", origin()).with_prop("format", Value::string("xml", origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("known formats: text, json")
        );
    }

    #[test]
    fn logging_the_query_string_has_to_be_asked_for() {
        let (config, _) = decode(Node::new("access-log", origin()));
        let mut with_query = Hasher::new();
        let mut without_query = Hasher::new();
        config.unwrap().hash_into(&mut without_query);
        decode(Node::new("access-log", origin()).with_prop("query", Value::bool(true, origin())))
            .0
            .unwrap()
            .hash_into(&mut with_query);
        assert_ne!(with_query.finish(), without_query.finish());
    }

    #[test]
    fn it_observes_and_never_rejects() {
        assert_eq!(AccessLog.meta().failure_policy, gatel_plan::FailurePolicy::Open);
        assert_eq!(
            AccessLog.meta().role,
            gatel_plan::Role::Middleware(Phase::Observe)
        );
    }

    #[test]
    fn outcomes_have_stable_names() {
        assert_eq!(Outcome::Completed.as_str(), "completed");
        assert_eq!(Outcome::Failed.as_str(), "body_error");
        assert_eq!(Outcome::Cancelled.as_str(), "cancelled");
    }
}
