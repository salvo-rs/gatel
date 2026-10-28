//! `redirect`: answer with a Location instead of content.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(arg)]
    to: String,
    /// `permanent` is opt-in because a wrong permanent redirect is cached by
    /// browsers and effectively cannot be taken back.
    #[gatel(prop, default = false)]
    permanent: bool,
    #[gatel(prop)]
    status: Option<u16>,
}

#[derive(Debug)]
pub struct Config {
    to: String,
    status: StatusCode,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.to);
        hasher.update_u64(self.status.as_u16() as u64);
    }

    fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        let location = http::HeaderValue::from_str(&self.to)
            .map_err(|_| format!("`{}` cannot be sent as a Location header", self.to))?;
        Ok(Arc::new(RedirectHandler {
            location,
            status: self.status,
        }))
    }

    fn describe(&self) -> String {
        format!("redirect {} -> {}", self.status.as_u16(), self.to)
    }
}

pub struct Redirect;

impl Directive for Redirect {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("redirect");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;

        let status = match fields.status {
            Some(explicit) => match StatusCode::from_u16(explicit) {
                Ok(status) if status.is_redirection() => status,
                _ => {
                    diagnostics.error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("{explicit} is not a redirect status code"),
                    );
                    return None;
                }
            },
            // 308 and 307 keep the method and body, which is what an operator
            // means by "send this somewhere else".
            None if fields.permanent => StatusCode::PERMANENT_REDIRECT,
            None => StatusCode::TEMPORARY_REDIRECT,
        };
        if fields.status.is_some() && fields.permanent {
            diagnostics.push(
                gatel_model::Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`permanent` and `status` say the same thing two ways",
                )
                .with_help("keep `status`, which is the more precise one"),
            );
            return None;
        }
        if fields.to.trim().is_empty() {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "a redirect needs a target",
            );
            return None;
        }

        Some(Arc::new(Config {
            to: fields.to,
            status,
        }))
    }
}

struct RedirectHandler {
    location: http::HeaderValue,
    status: StatusCode,
}

#[async_trait]
impl Handler for RedirectHandler {
    async fn handle(
        &self,
        _request: &mut Request,
        _depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        response.status_code(self.status);
        response
            .headers_mut()
            .insert(http::header::LOCATION, self.location.clone());
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    /// A context with nothing in it, for a directive that needs nothing.
    fn empty_context() -> gatel_plan::BuildContext<'static> {
        gatel_plan::BuildContext {
            log_sinks: None,
            secrets: Default::default(),
            blocking: gatel_plan::blocking::Executor::process_default(),
            telemetry: crate::testing::telemetry(),
            upstreams: crate::testing::no_upstreams(),
            root: std::path::Path::new("."),
            state: None,
        }
    }
    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Redirect.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn node(to: &str) -> Node {
        Node::new("redirect", origin()).with_arg(Value::string(to, origin()))
    }

    #[test]
    fn a_redirect_is_temporary_unless_asked_otherwise() {
        let (config, diagnostics) = decode(node("https://example.com/"));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert!(config.unwrap().describe().starts_with("redirect 307"));
    }

    #[test]
    fn permanent_uses_the_method_preserving_code() {
        let raw = node("https://example.com/").with_prop("permanent", Value::bool(true, origin()));
        let (config, _) = decode(raw);
        assert!(config.unwrap().describe().starts_with("redirect 308"));
    }

    #[test]
    fn a_non_redirect_status_is_rejected() {
        let raw = node("https://example.com/").with_prop("status", Value::int(200, origin()));
        let (config, diagnostics) = decode(raw);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("not a redirect status"))
        );
    }

    #[test]
    fn saying_it_twice_is_refused_rather_than_resolved_silently() {
        let raw = node("https://example.com/")
            .with_prop("permanent", Value::bool(true, origin()))
            .with_prop("status", Value::int(301, origin()));
        let (config, diagnostics) = decode(raw);
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("two ways")));
    }

    #[test]
    fn a_target_that_cannot_be_a_header_fails_at_build_not_at_request_time() {
        let (config, _) = decode(node("https://example.com/\u{7f}bad"));
        let error = match config.unwrap().build(empty_context()) {
            Ok(_) => panic!("a header value that cannot be sent must fail at build time"),
            Err(error) => error,
        };
        assert!(error.contains("Location"));
    }
}
