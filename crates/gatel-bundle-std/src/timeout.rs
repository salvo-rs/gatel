//! `timeout`: bound how long a route may take to decide on an answer.
//!
//! To the *head*, not to the last byte of the body. Once a handler has returned
//! and a streaming body is being written, cutting it off would truncate a
//! response the client is already reading — and a slow download is not the same
//! problem as a route that has hung. The body has its own bounds: the upstream
//! timeouts for a proxy, the tunnel watchdog for an upgraded connection.

use std::sync::Arc;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(arg)]
    after: Duration,
    /// What to answer when it runs out. 504 says the gateway did not get an
    /// answer in time, which is what happened.
    #[gatel(prop, default = 504)]
    status: u16,
}

#[derive(Debug)]
pub struct Config {
    after: Duration,
    status: StatusCode,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_duration(self.after);
        hasher.update_u64(self.status.as_u16() as u64);
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(TimeoutHandler {
            after: self.after,
            status: self.status,
        }))
    }

    fn describe(&self) -> String {
        format!("timeout {:?} -> {}", self.after, self.status.as_u16())
    }
}

pub struct Timeout;

impl Directive for Timeout {
    fn meta(&self) -> &DirectiveMeta {
        // A guard: it decides whether the request gets an answer from the route
        // at all. Ordered ahead of the other guards rather than left to fall
        // alphabetically among them, because "this route has thirty seconds"
        // means the whole route — including an authentication check that has to
        // ask something else.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("timeout", Phase::Guard, Scopes::BOTH).with_order(-100);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.after.is_zero() {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "a timeout of zero would refuse every request",
            );
            return None;
        }
        let Ok(status) = StatusCode::from_u16(fields.status) else {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                format!("{} is not a status code", fields.status),
            );
            return None;
        };
        Some(Arc::new(Config {
            after: fields.after,
            status,
        }))
    }
}

struct TimeoutHandler {
    after: Duration,
    status: StatusCode,
}

#[async_trait]
impl Handler for TimeoutHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let rest = ctrl.call_next(request, depot, response);
        if tokio::time::timeout(self.after, rest).await.is_ok() {
            return;
        }
        // Whatever the rest of the chain had started to write is discarded:
        // nothing has been sent, because a response only leaves once the chain
        // has returned, and a half-built one would be worse than none.
        crate::generated(depot);
        response.status_code(self.status);
        *response.body_mut() = salvo_core::http::ResBody::None;
        let _ = response.write_body(format!(
            "{} the route did not answer within {:?}\n",
            self.status.as_u16(),
            self.after
        ));
        ctrl.skip_rest();
        tracing::info!(
            target: "gatel::request",
            event = "route_timeout",
            after = ?self.after,
        );
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn node(after: &str) -> Node {
        Node::new("timeout", origin()).with_arg(Value::string(after, origin()))
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Timeout.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn handler(after: &str) -> Arc<dyn Handler> {
        decode(node(after))
            .0
            .unwrap()
            .build(BuildContext {
                secrets: Default::default(),
                blocking: gatel_plan::blocking::Executor::process_default(),
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap()
    }

    /// A handler that takes as long as it is told to.
    struct Slow(Duration);

    #[async_trait]
    impl Handler for Slow {
        async fn handle(
            &self,
            _request: &mut Request,
            _depot: &mut Depot,
            response: &mut Response,
            _ctrl: &mut FlowCtrl,
        ) {
            tokio::time::sleep(self.0).await;
            response.status_code(StatusCode::OK);
            let _ = response.write_body("finished\n");
        }
    }

    async fn answer(handler: Arc<dyn Handler>, inner: Duration) -> Response {
        let mut response = Response::new();
        let mut ctrl = FlowCtrl::new(vec![handler, Arc::new(Slow(inner))]);
        ctrl.call_next(&mut Request::default(), &mut Depot::new(), &mut response)
            .await;
        response
    }

    fn body(response: &mut Response) -> String {
        match std::mem::take(response.body_mut()) {
            salvo_core::http::ResBody::Once(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_that_answers_in_time_is_untouched() {
        let mut response = answer(handler("30s"), Duration::from_secs(1)).await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(body(&mut response), "finished\n");
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_that_hangs_is_answered_for() {
        let mut response = answer(handler("1s"), Duration::from_secs(60)).await;
        assert_eq!(response.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
        assert!(body(&mut response).contains("did not answer within"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_partly_built_response_is_discarded_rather_than_sent() {
        // The slow handler writes its body only after sleeping, so anything a
        // previous directive left behind must not survive the timeout either.
        let mut response = answer(handler("1s"), Duration::from_secs(60)).await;
        let text = body(&mut response);
        assert!(!text.contains("finished"), "{text}");
    }

    #[test]
    fn a_timeout_of_zero_is_refused() {
        let (config, diagnostics) = decode(node("0s"));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("refuse every request"))
        );
    }

    #[test]
    fn the_status_can_be_chosen() {
        let node = node("5s").with_prop("status", Value::int(503, origin()));
        let (config, _) = decode(node);
        assert!(config.unwrap().describe().ends_with("503"));
    }

    #[test]
    fn it_wraps_everything_the_route_does() {
        assert_eq!(Timeout.meta().role, gatel_plan::Role::Middleware(Phase::Guard));
        // Ahead of every other guard, whatever it is called.
        assert!(Timeout.meta().order < 0);
    }
}
