//! `trailing-slash`: settle on one spelling of a path.
//!
//! A redirect rather than a rewrite. Two URLs that answer the same content are
//! two URLs to a cache, a search engine and a set of relative links, and the
//! way to have one is to say which one it is.
//!
//! A path whose last segment looks like a filename is left alone. `add` would
//! otherwise turn `/style.css` into `/style.css/`, which no server anywhere
//! serves.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `/about` becomes `/about/`.
    Add,
    /// `/about/` becomes `/about`.
    Remove,
}

impl Style {
    const fn as_str(self) -> &'static str {
        match self {
            Style::Add => "add",
            Style::Remove => "remove",
        }
    }
}

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(arg)]
    style: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    style: Style,
}

impl Config {
    /// The path this one should be spelled as, if it is spelled differently.
    fn canonical(&self, path: &str) -> Option<String> {
        // The root is `/` under either style, and has no other spelling.
        if path == "/" || path.is_empty() {
            return None;
        }
        match self.style {
            Style::Add if !path.ends_with('/') => {
                looks_like_a_file(path).then_some(()).map_or_else(
                    || Some(format!("{path}/")),
                    // A file has an extension, not a directory listing.
                    |()| None,
                )
            }
            Style::Remove if path.ends_with('/') => Some(path.trim_end_matches('/').to_string()),
            _ => None,
        }
    }
}

/// Whether the last segment reads as a filename rather than a collection.
fn looks_like_a_file(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.') && !last.starts_with('.'))
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.style.as_str());
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(TrailingSlashHandler {
            config: Config { style: self.style },
        }))
    }

    fn describe(&self) -> String {
        format!("trailing-slash {}", self.style.as_str())
    }
}

pub struct TrailingSlash;

impl Directive for TrailingSlash {
    fn meta(&self) -> &DirectiveMeta {
        // A guard, because it answers instead of the route rather than changing
        // what the route sees.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("trailing-slash", Phase::Guard, Scopes::BOTH).with_version("2");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        let style = match fields.style.as_deref() {
            Some("add") => Style::Add,
            Some("remove") => Style::Remove,
            other => {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        match other {
                            Some(other) => format!("unknown trailing slash style `{other}`"),
                            None => "`trailing-slash` needs `add` or `remove`".to_string(),
                        },
                    )
                    .with_help("`trailing-slash \"add\"` or `trailing-slash \"remove\"`"),
                );
                return None;
            }
        };
        Some(Arc::new(Config { style }))
    }
}

struct TrailingSlashHandler {
    config: Config,
}

#[async_trait]
impl Handler for TrailingSlashHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let Some(canonical) = self.config.canonical(request.uri().path()) else {
            ctrl.call_next(request, depot, response).await;
            return;
        };
        let Some(location) = crate::local_redirect::location(&canonical, request.uri().query()) else {
            ctrl.call_next(request, depot, response).await;
            return;
        };
        // 308 rather than 301: a POST to the other spelling should arrive as a
        // POST, not be quietly turned into a GET.
        response.status_code(StatusCode::PERMANENT_REDIRECT);
        response.headers_mut().insert(http::header::LOCATION, location);
        ctrl.skip_rest();
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(style: Option<&str>) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut node = Node::new("trailing-slash", origin());
        if let Some(style) = style {
            node = node.with_arg(Value::string(style, origin()));
        }
        let mut diagnostics = Diagnostics::new();
        let config = TrailingSlash.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn config(style: Style) -> Config {
        Config { style }
    }

    #[test]
    fn adding_a_slash_leaves_the_root_alone() {
        let config = config(Style::Add);
        assert_eq!(config.canonical("/about"), Some("/about/".to_string()));
        assert_eq!(config.canonical("/about/"), None);
        assert_eq!(config.canonical("/"), None, "the root has one spelling");
    }

    #[test]
    fn removing_a_slash_leaves_the_root_alone() {
        let config = config(Style::Remove);
        assert_eq!(config.canonical("/about/"), Some("/about".to_string()));
        assert_eq!(config.canonical("/about"), None);
        assert_eq!(config.canonical("/"), None);
    }

    #[test]
    fn a_path_that_names_a_file_is_never_given_a_slash() {
        // `/style.css/` is not a thing any server serves.
        let config = config(Style::Add);
        assert_eq!(config.canonical("/assets/style.css"), None);
        assert_eq!(config.canonical("/logo.svg"), None);
        assert_eq!(
            config.canonical("/v1.2/docs"),
            Some("/v1.2/docs/".to_string()),
            "a dot earlier in the path is not a filename"
        );
    }

    #[test]
    fn a_dotfile_is_a_directory_as_far_as_this_can_tell() {
        // `.well-known` is a directory, and it is the case that matters.
        assert_eq!(
            config(Style::Add).canonical("/.well-known"),
            Some("/.well-known/".to_string())
        );
    }

    #[tokio::test]
    async fn a_redirect_keeps_the_query_and_the_method() {
        let (config, _) = decode(Some("add"));
        let handler = config
            .unwrap()
            .build(BuildContext {
                secrets: Default::default(),
                blocking: gatel_plan::blocking::Executor::process_default(),
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap();

        let mut request = Request::default();
        *request.method_mut() = http::Method::POST;
        *request.uri_mut() = "/about?page=2".parse().unwrap();
        let mut response = Response::new();
        handler
            .handle(
                &mut request,
                &mut Depot::new(),
                &mut response,
                &mut FlowCtrl::new(Vec::new()),
            )
            .await;

        assert_eq!(response.status_code, Some(StatusCode::PERMANENT_REDIRECT));
        assert_eq!(
            response
                .headers()
                .get(http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/about/?page=2"
        );
    }

    #[test]
    fn a_style_must_be_named() {
        let (config, diagnostics) = decode(None);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("needs `add` or `remove`"))
        );
    }

    #[test]
    fn an_unknown_style_is_refused() {
        let (config, diagnostics) = decode(Some("strip"));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("unknown trailing slash"))
        );
    }
}
