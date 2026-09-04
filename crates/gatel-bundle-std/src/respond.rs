//! `respond`: answer directly, without a backend.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// A response body written into configuration has to be read by a person, so
/// it is bounded well below what a file could hold.
const MAX_BODY: ByteSize = ByteSize(64 * 1024);

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(arg, default)]
    body: String,
    #[gatel(prop, default = 200)]
    status: u16,
    #[gatel(prop)]
    content_type: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    status: StatusCode,
    body: String,
    content_type: Option<String>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.status.as_u16() as u64);
        hasher.update_str(&self.body);
        hasher.update_opt_str(self.content_type.as_deref());
    }

    fn build(
        &self,
        _state: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(RespondHandler {
            status: self.status,
            body: self.body.clone(),
            content_type: self.content_type.clone(),
        }))
    }

    fn describe(&self) -> String {
        let preview: String = self.body.chars().take(32).collect();
        format!("respond {} {preview:?}", self.status.as_u16())
    }
}

pub struct Respond;

impl Directive for Respond {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("respond");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;

        // `http` accepts 100..=999; HTTP only defines 100..=599, and a code
        // outside that range is a typo rather than an intention.
        let status = match StatusCode::from_u16(fields.status) {
            Ok(status) if (100..=599).contains(&fields.status) => status,
            _ => {
                diagnostics.error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("{} is not an HTTP status code", fields.status),
                );
                return None;
            }
        };
        if fields.body.len() as u64 > MAX_BODY.bytes() {
            diagnostics.push(
                gatel_model::Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("a `respond` body may not exceed {MAX_BODY}"),
                )
                .with_help("serve larger content from a file instead"),
            );
            return None;
        }
        // A body on a status that must not have one would either be dropped or
        // break framing, so it is refused rather than silently ignored.
        if !fields.body.is_empty() && matches!(fields.status, 204 | 304) {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                format!("a {} response cannot carry a body", fields.status),
            );
            return None;
        }

        Some(Arc::new(Config {
            status,
            body: fields.body,
            content_type: fields.content_type,
        }))
    }
}

struct RespondHandler {
    status: StatusCode,
    body: String,
    content_type: Option<String>,
}

#[async_trait]
impl Handler for RespondHandler {
    async fn handle(
        &self,
        request: &mut Request,
        _depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        response.status_code(self.status);
        let content_type = self
            .content_type
            .as_deref()
            .unwrap_or("text/plain; charset=utf-8");
        if let Ok(value) = http::HeaderValue::from_str(content_type) {
            response.headers_mut().insert(http::header::CONTENT_TYPE, value);
        }
        // A HEAD response carries the headers of the GET it stands for, but no
        // body; hyper computes the length from what we would have written.
        if request.method() == http::Method::HEAD {
            return;
        }
        let _ = response.write_body(self.body.clone());
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
        let config = Respond.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    #[test]
    fn the_default_is_an_empty_two_hundred() {
        let (config, diagnostics) = decode(Node::new("respond", origin()));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "respond 200 \"\"");
    }

    #[test]
    fn an_impossible_status_code_is_rejected() {
        for code in [99, 600, 999, 70_000] {
            let node = Node::new("respond", origin()).with_prop("status", Value::int(code, origin()));
            let (config, diagnostics) = decode(node);
            assert!(config.is_none(), "{code} should not be accepted");
            assert!(diagnostics.has_errors());
        }
        let node = Node::new("respond", origin()).with_prop("status", Value::int(418, origin()));
        assert!(
            decode(node).0.is_some(),
            "an unusual but real status code is fine"
        );
    }

    #[test]
    fn a_body_on_a_bodiless_status_is_rejected() {
        let node = Node::new("respond", origin())
            .with_arg(Value::string("content", origin()))
            .with_prop("status", Value::int(204, origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot carry a body"))
        );
    }

    #[test]
    fn an_oversized_body_points_at_the_file_server() {
        let node = Node::new("respond", origin()).with_arg(Value::string("x".repeat(70_000), origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("file")
        );
    }

    #[test]
    fn identical_configurations_hash_the_same_and_different_ones_do_not() {
        let hash = |body: &str| {
            let node = Node::new("respond", origin()).with_arg(Value::string(body, origin()));
            let mut hasher = Hasher::new();
            decode(node).0.unwrap().hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash("a"), hash("a"));
        assert_ne!(hash("a"), hash("b"));
    }
}
