//! `headers`: add, replace or remove request and response headers.
//!
//! An around middleware: the `request` operations run before the terminal, the
//! `response` operations after it. Because it sits in the response phase, a
//! site-scoped `headers` wraps a route-scoped one, so the outermost declaration
//! sees the response last.

use std::sync::Arc;

use gatel_model::{Code, Diagnostic, Diagnostics, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// Headers the gateway owns. Letting configuration change these would break
/// framing or routing in ways that look like a bug elsewhere.
const PROTECTED: [&str; 8] = [
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

const MAX_OPERATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    /// Replace every existing value.
    Set {
        name: String,
        value: String,
    },
    /// Add another value, keeping any that are there.
    Add {
        name: String,
        value: String,
    },
    Remove {
        name: String,
    },
}

impl Operation {
    fn hash_into(&self, hasher: &mut Hasher) {
        match self {
            Operation::Set { name, value } => {
                hasher.update_str("set");
                hasher.update_str(name);
                hasher.update_str(value);
            }
            Operation::Add { name, value } => {
                hasher.update_str("add");
                hasher.update_str(name);
                hasher.update_str(value);
            }
            Operation::Remove { name } => {
                hasher.update_str("remove");
                hasher.update_str(name);
            }
        }
    }
}

#[derive(Debug)]
pub struct Config {
    request: Vec<Operation>,
    response: Vec<Operation>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        // Order matters: `set` then `add` differs from `add` then `set`.
        hasher.update_ordered(&self.request, |operation, hasher| operation.hash_into(hasher));
        hasher.update_ordered(&self.response, |operation, hasher| operation.hash_into(hasher));
    }

    fn build(
        &self,
        _state: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(HeadersHandler {
            request: compile(&self.request)?,
            response: compile(&self.response)?,
        }))
    }

    fn describe(&self) -> String {
        format!(
            "headers {} request, {} response",
            self.request.len(),
            self.response.len()
        )
    }
}

pub struct Headers;

impl Directive for Headers {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("headers", Phase::Response, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let errors_before = diagnostics.error_count();
        let mut request = Vec::new();
        let mut response = Vec::new();

        for child in &node.children {
            let target = match child.name.as_str() {
                "request" => &mut request,
                "response" => &mut response,
                other => {
                    let mut diagnostic = Diagnostic::error(
                        Code::UnknownNode,
                        child.origin.clone(),
                        format!("`headers` has no child node `{other}`"),
                    );
                    if let Some(suggestion) = gatel_model::did_you_mean(other, ["request", "response"]) {
                        diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                    }
                    diagnostics.push(diagnostic);
                    continue;
                }
            };
            decode_operations(child, target, diagnostics);
        }

        for property in &node.props {
            diagnostics.error(
                Code::UnknownProperty,
                property.origin.clone(),
                format!("`headers` has no property `{}`", property.name),
            );
        }
        if request.is_empty() && response.is_empty() && diagnostics.error_count() == errors_before {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`headers` does nothing without a `request` or `response` block",
                )
                .with_help("write `headers { response { set \"x-name\" \"value\" } }`"),
            );
        }
        if request.len() + response.len() > MAX_OPERATIONS {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                format!("a `headers` block may hold at most {MAX_OPERATIONS} operations"),
            );
        }

        if diagnostics.error_count() > errors_before {
            return None;
        }
        Some(Arc::new(Config { request, response }))
    }
}

fn decode_operations(node: &Node, into: &mut Vec<Operation>, diagnostics: &mut Diagnostics) {
    for child in &node.children {
        let arguments: Vec<&str> = child.args.iter().filter_map(|value| value.as_str()).collect();
        if arguments.len() != child.args.len() {
            diagnostics.error(
                Code::InvalidValue,
                child.origin.clone(),
                "header names and values are strings",
            );
            continue;
        }
        let Some(name) = arguments.first() else {
            diagnostics.error(
                Code::ArgumentCount,
                child.origin.clone(),
                format!("`{}` needs a header name", child.name),
            );
            continue;
        };
        let lowered = name.to_ascii_lowercase();

        if http::HeaderName::try_from(lowered.as_str()).is_err() {
            diagnostics.error(
                Code::InvalidValue,
                child.origin.clone(),
                format!("`{name}` is not a valid header name"),
            );
            continue;
        }
        if PROTECTED.contains(&lowered.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    child.origin.clone(),
                    format!("`{lowered}` is owned by the gateway and cannot be changed"),
                )
                .with_help("it controls framing or routing, which configuration must not reach past"),
            );
            continue;
        }

        let operation = match (child.name.as_str(), arguments.len()) {
            ("remove", 1) => Operation::Remove { name: lowered },
            ("set", 2) => Operation::Set {
                name: lowered,
                value: arguments[1].to_string(),
            },
            ("add", 2) => Operation::Add {
                name: lowered,
                value: arguments[1].to_string(),
            },
            ("set" | "add", _) => {
                diagnostics.error(
                    Code::ArgumentCount,
                    child.origin.clone(),
                    format!("`{}` takes a name and a value", child.name),
                );
                continue;
            }
            ("remove", _) => {
                diagnostics.error(
                    Code::ArgumentCount,
                    child.origin.clone(),
                    "`remove` takes only a name",
                );
                continue;
            }
            (other, _) => {
                let mut diagnostic = Diagnostic::error(
                    Code::UnknownNode,
                    child.origin.clone(),
                    format!("`{other}` is not a header operation"),
                );
                if let Some(suggestion) = gatel_model::did_you_mean(other, ["set", "add", "remove"]) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                }
                diagnostics.push(diagnostic);
                continue;
            }
        };
        if let Operation::Set { value, .. } | Operation::Add { value, .. } = &operation
            && http::HeaderValue::from_str(value).is_err()
        {
            diagnostics.error(
                Code::InvalidValue,
                child.origin.clone(),
                format!("`{value}` cannot be sent as a header value"),
            );
            continue;
        }
        into.push(operation);
    }
}

enum Compiled {
    Set(http::HeaderName, http::HeaderValue),
    Add(http::HeaderName, http::HeaderValue),
    Remove(http::HeaderName),
}

/// Turn the configuration into pre-parsed header types once, at prepare time,
/// so a request never parses a name or value.
fn compile(operations: &[Operation]) -> Result<Vec<Compiled>, String> {
    operations
        .iter()
        .map(|operation| {
            let name = |name: &String| {
                http::HeaderName::try_from(name.as_str())
                    .map_err(|_| format!("`{name}` is not a valid header name"))
            };
            let value = |value: &String| {
                http::HeaderValue::from_str(value)
                    .map_err(|_| format!("`{value}` is not a valid header value"))
            };
            Ok(match operation {
                Operation::Set { name: n, value: v } => Compiled::Set(name(n)?, value(v)?),
                Operation::Add { name: n, value: v } => Compiled::Add(name(n)?, value(v)?),
                Operation::Remove { name: n } => Compiled::Remove(name(n)?),
            })
        })
        .collect()
}

fn apply(operations: &[Compiled], headers: &mut http::HeaderMap) {
    for operation in operations {
        match operation {
            Compiled::Set(name, value) => {
                headers.insert(name.clone(), value.clone());
            }
            Compiled::Add(name, value) => {
                headers.append(name.clone(), value.clone());
            }
            Compiled::Remove(name) => {
                headers.remove(name);
            }
        }
    }
}

struct HeadersHandler {
    request: Vec<Compiled>,
    response: Vec<Compiled>,
}

#[async_trait]
impl Handler for HeadersHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        apply(&self.request, request.headers_mut());
        ctrl.call_next(request, depot, response).await;
        apply(&self.response, response.headers_mut());
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn operation(kind: &str, arguments: &[&str]) -> Node {
        let mut node = Node::new(kind, origin());
        for argument in arguments {
            node = node.with_arg(Value::string(*argument, origin()));
        }
        node
    }

    fn headers_node(block: &str, operations: Vec<Node>) -> Node {
        let mut inner = Node::new(block, origin());
        inner.children = operations;
        Node::new("headers", origin()).with_child(inner)
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Headers.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    #[test]
    fn set_add_and_remove_all_decode() {
        let node = headers_node(
            "response",
            vec![
                operation("set", &["X-One", "1"]),
                operation("add", &["x-two", "2"]),
                operation("remove", &["x-three"]),
            ],
        );
        let (config, diagnostics) = decode(node);
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "headers 0 request, 3 response");
    }

    #[test]
    fn a_framing_header_cannot_be_changed() {
        let node = headers_node("response", vec![operation("set", &["Content-Length", "10"])]);
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        let diagnostic = diagnostics.iter().next().unwrap();
        assert!(diagnostic.message.contains("owned by the gateway"));
        assert!(diagnostic.help.is_some());
    }

    #[test]
    fn an_empty_headers_block_is_refused_rather_than_ignored() {
        let (config, diagnostics) = decode(Node::new("headers", origin()));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("does nothing")));
    }

    #[test]
    fn an_unknown_operation_suggests_a_known_one() {
        let node = headers_node("response", vec![operation("sett", &["x", "1"])]);
        let (_, diagnostics) = decode(node);
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("did you mean `set`?")
        );
    }

    #[test]
    fn an_unknown_block_suggests_a_known_one() {
        let node = Node::new("headers", origin()).with_child(Node::new("responses", origin()));
        let (_, diagnostics) = decode(node);
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("did you mean `response`?")
        );
    }

    #[test]
    fn header_names_are_case_insensitive_in_the_fingerprint() {
        let hash = |name: &str| {
            let node = headers_node("response", vec![operation("set", &[name, "1"])]);
            let mut hasher = Hasher::new();
            decode(node).0.unwrap().hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash("X-Name"), hash("x-name"));
    }

    #[test]
    fn operation_order_is_part_of_the_fingerprint() {
        let hash = |operations: Vec<Node>| {
            let mut hasher = Hasher::new();
            decode(headers_node("response", operations))
                .0
                .unwrap()
                .hash_into(&mut hasher);
            hasher.finish()
        };
        let set_then_add = vec![operation("set", &["x", "1"]), operation("add", &["x", "2"])];
        let add_then_set = vec![operation("add", &["x", "2"]), operation("set", &["x", "1"])];
        assert_ne!(hash(set_then_add), hash(add_then_set));
    }

    #[test]
    fn a_value_that_cannot_be_sent_is_rejected_at_compile_time() {
        let node = headers_node("response", vec![operation("set", &["x-name", "bad\nvalue"])]);
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("cannot be sent")));
    }
}
