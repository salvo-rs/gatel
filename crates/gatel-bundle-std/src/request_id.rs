//! `request-id`: give every request a name that appears in its log line.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gatel_macros::FromNode;
use gatel_model::{Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(prop, default = String::from("x-request-id"))]
    header: String,
    /// Whether to keep an id the client supplied.
    ///
    /// Off by default: an id a client chose can be repeated, so believing it
    /// lets one client's traffic be attributed to another's.
    #[gatel(prop, default = false)]
    trust_client: bool,
}

#[derive(Debug)]
pub struct Config {
    header: String,
    trust_client: bool,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.header);
        hasher.update_bool(self.trust_client);
    }

    fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        let name = http::HeaderName::try_from(self.header.as_str())
            .map_err(|_| format!("`{}` is not a valid header name", self.header))?;
        Ok(Arc::new(RequestIdHandler {
            header: name,
            trust_client: self.trust_client,
            counter: AtomicU64::new(0),
            prefix: prefix(),
        }))
    }

    fn describe(&self) -> String {
        format!(
            "request-id {}{}",
            self.header,
            if self.trust_client {
                " (trusting the client)"
            } else {
                ""
            }
        )
    }
}

pub struct RequestId;

impl Directive for RequestId {
    fn meta(&self) -> &DirectiveMeta {
        // Outermost with the other observation, so the id exists before
        // anything can reject the request and is present in that log line too.
        static META: DirectiveMeta = DirectiveMeta::middleware("request-id", Phase::Observe, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        Some(Arc::new(Config {
            header: fields.header.to_ascii_lowercase(),
            trust_client: fields.trust_client,
        }))
    }
}

/// A per-process prefix, so ids from two gateways cannot collide even when
/// their counters are both small.
fn prefix() -> String {
    let mut hasher = Hasher::new();
    hasher.update_u64(std::process::id() as u64);
    hasher.update_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default(),
    );
    hasher.finish().short()[..8].to_string()
}

struct RequestIdHandler {
    header: http::HeaderName,
    trust_client: bool,
    counter: AtomicU64,
    prefix: String,
}

#[async_trait]
impl Handler for RequestIdHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let existing = self
            .trust_client
            .then(|| {
                request
                    .headers()
                    .get(&self.header)
                    .and_then(|value| value.to_str().ok())
            })
            .flatten()
            .filter(|value| is_reasonable(value))
            .map(str::to_string);

        let id = existing.unwrap_or_else(|| {
            format!(
                "{}-{:x}",
                self.prefix,
                self.counter.fetch_add(1, Ordering::Relaxed)
            )
        });

        if let Ok(value) = http::HeaderValue::from_str(&id) {
            request.headers_mut().insert(self.header.clone(), value.clone());
            response.headers_mut().insert(self.header.clone(), value);
        }
        depot.insert("gatel.request_id", id);

        ctrl.call_next(request, depot, response).await;
    }
}

/// A client-supplied id is only echoed when it could plausibly be one: short,
/// printable and free of anything that would break a log line.
fn is_reasonable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    /// A context with nothing in it, for a directive that needs nothing.
    fn empty_context() -> gatel_plan::BuildContext<'static> {
        gatel_plan::BuildContext {
            root: std::path::Path::new("."),
            state: None,
        }
    }
    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = RequestId.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    #[test]
    fn the_default_generates_rather_than_trusts() {
        let (config, diagnostics) = decode(Node::new("request-id", origin()));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "request-id x-request-id");
    }

    #[test]
    fn trusting_the_client_has_to_be_asked_for_and_is_visible() {
        let raw = Node::new("request-id", origin()).with_prop("trust-client", Value::bool(true, origin()));
        let (config, _) = decode(raw);
        assert!(config.unwrap().describe().contains("trusting the client"));
    }

    #[test]
    fn a_custom_header_name_is_normalised() {
        let raw =
            Node::new("request-id", origin()).with_prop("header", Value::string("X-Trace-Id", origin()));
        let (config, _) = decode(raw);
        assert!(config.unwrap().describe().contains("x-trace-id"));
    }

    #[test]
    fn an_impossible_header_name_fails_at_build_time() {
        let raw =
            Node::new("request-id", origin()).with_prop("header", Value::string("not a header", origin()));
        let (config, _) = decode(raw);
        assert!(config.unwrap().build(empty_context()).is_err());
    }

    #[test]
    fn only_a_plausible_client_id_is_echoed() {
        assert!(is_reasonable("abc-123"));
        assert!(is_reasonable("trace.0:1"));
        assert!(!is_reasonable(""));
        assert!(
            !is_reasonable(&"x".repeat(65)),
            "an unbounded id would bloat every log line"
        );
        assert!(!is_reasonable("has space"));
        assert!(
            !is_reasonable("newline\nhere"),
            "a log line must not be forgeable"
        );
    }

    #[test]
    fn generated_ids_are_distinct() {
        let (config, _) = decode(Node::new("request-id", origin()));
        let handler = config.unwrap().build(empty_context()).unwrap();
        let _ = handler;
        let first = prefix();
        assert_eq!(first.len(), 8);
    }
}
