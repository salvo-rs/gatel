//! `rewrite`: change the path before the terminal sees it.
//!
//! Deliberately not a regular expression. The three shapes here cover what a
//! gateway in front of a backend actually needs, and each is obvious from
//! reading it — which a pattern with capture groups in a configuration file is
//! not.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    /// Remove this from the front of the path: `/api/users` becomes `/users`.
    #[gatel(prop)]
    strip_prefix: Option<String>,
    /// Add this to the front, after any stripping.
    #[gatel(prop)]
    prefix: Option<String>,
    /// Replace the path outright, whatever it was.
    #[gatel(prop)]
    to: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    strip_prefix: Option<String>,
    prefix: Option<String>,
    to: Option<String>,
}

impl Config {
    /// The path a request should carry onward.
    ///
    /// Returns `None` when nothing changes, so a request that would be
    /// rewritten to itself is not rebuilt.
    fn apply(&self, path: &str) -> Option<String> {
        if let Some(to) = &self.to {
            return (to != path).then(|| to.clone());
        }
        let mut rewritten = path.to_string();
        if let Some(strip) = &self.strip_prefix
            && let Some(rest) = rewritten.strip_prefix(strip.as_str())
        {
            // Stripping `/api` from `/api` leaves nothing, which is not a path.
            rewritten = if rest.starts_with('/') {
                rest.to_string()
            } else {
                format!("/{rest}")
            };
        }
        if let Some(prefix) = &self.prefix {
            rewritten = format!("{}{}", prefix.trim_end_matches('/'), rewritten);
        }
        (rewritten != path).then_some(rewritten)
    }
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_opt_str(self.strip_prefix.as_deref());
        hasher.update_opt_str(self.prefix.as_deref());
        hasher.update_opt_str(self.to.as_deref());
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(RewriteHandler {
            config: Config {
                strip_prefix: self.strip_prefix.clone(),
                prefix: self.prefix.clone(),
                to: self.to.clone(),
            },
        }))
    }

    fn describe(&self) -> String {
        match (&self.to, &self.strip_prefix, &self.prefix) {
            (Some(to), ..) => format!("rewrite to {to}"),
            (None, Some(strip), Some(prefix)) => format!("rewrite {strip} -> {prefix}"),
            (None, Some(strip), None) => format!("rewrite strip {strip}"),
            (None, None, Some(prefix)) => format!("rewrite prefix {prefix}"),
            (None, None, None) => "rewrite (nothing)".to_string(),
        }
    }
}

pub struct Rewrite;

impl Directive for Rewrite {
    fn meta(&self) -> &DirectiveMeta {
        // The request phase: after the guards have decided whether to allow the
        // request at all, and before the terminal reads its path. Rewriting
        // before a guard would let a rewritten path slip past a filter written
        // against the original one.
        static META: DirectiveMeta = DirectiveMeta::middleware("rewrite", Phase::Request, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;

        if fields.to.is_some() && (fields.strip_prefix.is_some() || fields.prefix.is_some()) {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`to` replaces the whole path, so it cannot be combined with `strip-prefix` or `prefix`",
                )
                .with_help("use one or the other"),
            );
            return None;
        }
        if fields.to.is_none() && fields.strip_prefix.is_none() && fields.prefix.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`rewrite` needs `strip-prefix`, `prefix` or `to`",
                )
                .with_help("a rewrite that changes nothing is a mistake, not a no-op"),
            );
            return None;
        }
        for (name, value) in [
            ("strip-prefix", &fields.strip_prefix),
            ("prefix", &fields.prefix),
            ("to", &fields.to),
        ] {
            if let Some(value) = value
                && !value.starts_with('/')
            {
                diagnostics.error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("`{name}` must start with `/`"),
                );
                return None;
            }
        }

        Some(Arc::new(Config {
            strip_prefix: fields.strip_prefix,
            prefix: fields.prefix,
            to: fields.to,
        }))
    }
}

struct RewriteHandler {
    config: Config,
}

#[async_trait]
impl Handler for RewriteHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if let Some(rewritten) = self.config.apply(request.uri().path()) {
            let query = request.uri().query();
            // The query survives a rewrite: it belongs to the request, not to
            // the path shape being changed.
            let target = match query {
                Some(query) => format!("{rewritten}?{query}"),
                None => rewritten,
            };
            match rebuild(request.uri(), &target) {
                Ok(uri) => *request.uri_mut() = uri,
                Err(error) => {
                    tracing::warn!(
                        target: "gatel::request",
                        event = "rewrite_failed",
                        detail = %error,
                    );
                }
            }
        }
        ctrl.call_next(request, depot, response).await;
    }
}

/// Replace the path and query of a URI, keeping whatever else it carried.
fn rebuild(original: &http::Uri, path_and_query: &str) -> Result<http::Uri, String> {
    let mut parts = original.clone().into_parts();
    parts.path_and_query = Some(
        path_and_query
            .parse()
            .map_err(|_| format!("`{path_and_query}` is not a valid path"))?,
    );
    http::Uri::from_parts(parts).map_err(|error| error.to_string())
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
        let config = Rewrite.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn node(pairs: &[(&str, &str)]) -> Node {
        let mut node = Node::new("rewrite", origin());
        for (name, value) in pairs {
            node = node.with_prop(*name, Value::string(*value, origin()));
        }
        node
    }

    fn config(pairs: &[(&str, &str)]) -> Config {
        let mut fields = Config {
            strip_prefix: None,
            prefix: None,
            to: None,
        };
        for (name, value) in pairs {
            match *name {
                "strip-prefix" => fields.strip_prefix = Some((*value).to_string()),
                "prefix" => fields.prefix = Some((*value).to_string()),
                _ => fields.to = Some((*value).to_string()),
            }
        }
        fields
    }

    #[test]
    fn stripping_a_prefix_leaves_a_usable_path() {
        let config = config(&[("strip-prefix", "/api")]);
        assert_eq!(config.apply("/api/users"), Some("/users".to_string()));
        // Stripping everything must not leave an empty path.
        assert_eq!(config.apply("/api"), Some("/".to_string()));
        assert_eq!(
            config.apply("/other"),
            None,
            "a path that does not match is left alone"
        );
    }

    #[test]
    fn adding_a_prefix_does_not_double_the_separator() {
        assert_eq!(
            config(&[("prefix", "/v2")]).apply("/users"),
            Some("/v2/users".to_string())
        );
        assert_eq!(
            config(&[("prefix", "/v2/")]).apply("/users"),
            Some("/v2/users".to_string())
        );
    }

    #[test]
    fn stripping_and_adding_compose_in_that_order() {
        let config = config(&[("strip-prefix", "/api"), ("prefix", "/internal")]);
        assert_eq!(config.apply("/api/users"), Some("/internal/users".to_string()));
    }

    #[test]
    fn replacing_the_path_ignores_what_was_there() {
        let config = config(&[("to", "/index.html")]);
        assert_eq!(config.apply("/anything/at/all"), Some("/index.html".to_string()));
        assert_eq!(
            config.apply("/index.html"),
            None,
            "rewriting to itself changes nothing"
        );
    }

    #[test]
    fn combining_a_replacement_with_a_prefix_is_refused() {
        let (config, diagnostics) = decode(node(&[("to", "/a"), ("prefix", "/b")]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot be combined"))
        );
    }

    #[test]
    fn a_rewrite_that_does_nothing_is_a_mistake() {
        let (config, diagnostics) = decode(Node::new("rewrite", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("no-op")
        );
    }

    #[test]
    fn a_path_without_a_leading_slash_is_refused() {
        let (config, diagnostics) = decode(node(&[("prefix", "v2")]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("must start with `/`"))
        );
    }

    #[test]
    fn the_query_string_survives_a_rewrite() {
        let uri: http::Uri = "http://example.com/api/users?page=2".parse().unwrap();
        let rebuilt = rebuild(&uri, "/users?page=2").unwrap();
        assert_eq!(rebuilt.path(), "/users");
        assert_eq!(rebuilt.query(), Some("page=2"));
        assert_eq!(rebuilt.host(), Some("example.com"), "the authority is untouched");
    }

    #[test]
    fn it_runs_after_guards_and_before_the_terminal() {
        // Rewriting before a guard would let a rewritten path slip past a
        // filter written against the original one.
        assert_eq!(Rewrite.meta().role, gatel_plan::Role::Middleware(Phase::Request));
        assert!(Phase::Guard < Phase::Request);
        assert!(Phase::Request < Phase::Response);
    }
}
