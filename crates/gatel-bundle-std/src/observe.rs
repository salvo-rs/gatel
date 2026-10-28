//! `metrics` and `health`: what the process says about itself.
//!
//! Both are ordinary terminals on ordinary routes, which is the whole point.
//! There is no separate admin port that quietly serves them on every interface,
//! no fixed path, and no exemption from the rest of the configuration — a
//! `metrics` route can be put behind `ip-filter` or `basic-auth`, bound to a
//! loopback listener, or given a name nobody guesses, and the same rules apply
//! to it as to anything else.

use std::sync::Arc;

use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_observe::Telemetry;
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// `metrics`: the Prometheus text exposition of everything measured.
#[derive(Debug)]
pub struct MetricsConfig;

impl DirectiveConfig for MetricsConfig {
    fn hash_into(&self, _hasher: &mut Hasher) {}

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(MetricsHandler {
            telemetry: Arc::clone(cx.telemetry),
            blocking: cx.blocking,
        }))
    }

    fn describe(&self) -> String {
        "metrics prometheus".to_string()
    }
}

pub struct Metrics;

impl Directive for Metrics {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("metrics").with_version("2");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        // Nothing to configure, but the node is still read, so a stray
        // property is an error rather than something silently ignored.
        gatel_model::NodeReader::new(node, diagnostics).finish();
        Some(Arc::new(MetricsConfig))
    }
}

struct MetricsHandler {
    telemetry: Arc<Telemetry>,
    blocking: gatel_plan::blocking::Executor,
}

#[async_trait]
impl Handler for MetricsHandler {
    async fn handle(
        &self,
        _request: &mut Request,
        _depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let mut body = gatel_observe::prometheus(&self.telemetry);
        gatel_observe::blocking::append_blocking(&mut body, self.blocking.metrics());
        response
            .add_header(http::header::CONTENT_TYPE, gatel_observe::CONTENT_TYPE, true)
            .ok();
        let _ = response.write_body(body);
    }
}

/// Which question a probe is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Should this instance be sent traffic?
    Ready,
    /// Would restarting this process help?
    Live,
}

impl Probe {
    const fn as_str(self) -> &'static str {
        match self {
            Probe::Ready => "ready",
            Probe::Live => "live",
        }
    }
}

#[derive(Debug, gatel_macros::FromNode)]
struct Fields {
    #[gatel(arg)]
    probe: Option<String>,
}

#[derive(Debug)]
pub struct HealthConfig {
    probe: Probe,
}

impl DirectiveConfig for HealthConfig {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.probe.as_str());
    }

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(HealthHandler {
            probe: self.probe,
            telemetry: Arc::clone(cx.telemetry),
        }))
    }

    fn describe(&self) -> String {
        format!("health {}", self.probe.as_str())
    }
}

pub struct Health;

impl Directive for Health {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("health");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        // No default: the two probes mean opposite things to an orchestrator,
        // and guessing which one an operator meant is not a favour.
        let probe = match fields.probe.as_deref() {
            Some("ready") => Probe::Ready,
            Some("live") => Probe::Live,
            other => {
                diagnostics.push(
                    gatel_model::Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        match other {
                            Some(other) => format!("unknown probe `{other}`"),
                            None => "`health` needs to be told which probe it answers".to_string(),
                        },
                    )
                    .with_help(
                        "`health \"ready\"` decides whether to send traffic here; \
                         `health \"live\"` decides whether to restart the process",
                    ),
                );
                return None;
            }
        };
        Some(Arc::new(HealthConfig { probe }))
    }
}

struct HealthHandler {
    probe: Probe,
    telemetry: Arc<Telemetry>,
}

#[async_trait]
impl Handler for HealthHandler {
    async fn handle(
        &self,
        _request: &mut Request,
        _depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        response
            .add_header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8", true)
            .ok();
        // Never cached: a probe answer that a proxy remembers is worse than no
        // probe at all.
        response
            .add_header(http::header::CACHE_CONTROL, "no-store", true)
            .ok();

        match self.probe {
            Probe::Live => {
                response.status_code(if self.telemetry.health.alive() {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                });
                let _ = response.write_body("alive\n");
            }
            Probe::Ready => {
                let report = self.telemetry.health.report();
                response.status_code(if report.state.is_serving() {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                });
                let _ = response.write_body(report.render());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};
    use gatel_observe::State;

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode<D: Directive>(directive: D, node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = directive.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn health(probe: &str) -> Node {
        Node::new("health", origin()).with_arg(Value::string(probe, origin()))
    }

    async fn answer(handler: &dyn Handler) -> (u16, String) {
        let mut response = Response::new();
        handler
            .handle(
                &mut Request::default(),
                &mut Depot::new(),
                &mut response,
                &mut FlowCtrl::new(Vec::new()),
            )
            .await;
        let status = response.status_code.map(|code| code.as_u16()).unwrap_or(200);
        let body = match std::mem::take(response.body_mut()) {
            salvo_core::http::ResBody::Once(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            other => format!("{other:?}"),
        };
        (status, body)
    }

    fn built(config: &dyn DirectiveConfig, telemetry: &'static Arc<Telemetry>) -> Arc<dyn Handler> {
        config
            .build(BuildContext {
                log_sinks: None,
                secrets: Default::default(),
                blocking: gatel_plan::blocking::Executor::process_default(),
                root: std::path::Path::new("."),
                state: None,
                telemetry,
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap()
    }

    #[tokio::test]
    async fn readiness_says_why_it_is_not_ready() {
        let telemetry = crate::testing::telemetry();
        let (config, _) = decode(Health, health("ready"));
        let handler = built(config.unwrap().as_ref(), telemetry);
        let (status, body) = answer(handler.as_ref()).await;
        // Nothing has been committed in this test binary.
        assert_eq!(status, 503);
        assert!(body.starts_with("not ready\n"), "{body}");
        assert!(body.contains("no configuration has been accepted"), "{body}");
    }

    #[tokio::test]
    async fn liveness_answers_even_when_readiness_will_not() {
        let telemetry = crate::testing::telemetry();
        assert_eq!(telemetry.health.report().state, State::NotReady);
        let (config, _) = decode(Health, health("live"));
        let handler = built(config.unwrap().as_ref(), telemetry);
        let (status, body) = answer(handler.as_ref()).await;
        assert_eq!(status, 200, "a starting process is not a wedged one");
        assert_eq!(body, "alive\n");
    }

    #[test]
    fn a_probe_must_be_named() {
        let (config, diagnostics) = decode(Health, Node::new("health", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("which probe it answers"))
        );
    }

    #[test]
    fn an_unknown_probe_is_refused_with_the_two_that_exist() {
        let (config, diagnostics) = decode(Health, health("healthy"));
        assert!(config.is_none());
        let help = diagnostics.iter().next().unwrap().help.clone().unwrap();
        assert!(help.contains("ready") && help.contains("live"), "{help}");
    }

    #[tokio::test]
    async fn metrics_renders_the_exposition_format() {
        let telemetry = crate::testing::telemetry();
        let (config, diagnostics) = decode(Metrics, Node::new("metrics", origin()));
        assert!(!diagnostics.has_errors());
        let handler = built(config.unwrap().as_ref(), telemetry);
        let (status, body) = answer(handler.as_ref()).await;
        assert_eq!(status, 200);
        assert!(body.contains("# TYPE gatel_generation gauge"), "{body}");
    }

    #[test]
    fn a_property_nobody_reads_is_an_error_rather_than_ignored() {
        let node = Node::new("metrics", origin()).with_prop("format", Value::string("json", origin()));
        let (_, diagnostics) = decode(Metrics, node);
        assert!(diagnostics.has_errors(), "an unknown property must be reported");
    }

    #[test]
    fn both_are_terminals_so_nothing_runs_after_them() {
        assert_eq!(Metrics.meta().role, gatel_plan::Role::Terminal);
        assert_eq!(Health.meta().role, gatel_plan::Role::Terminal);
    }
}
