//! `error-pages`: replace the body of an error Gatel produced.
//!
//! Only one Gatel produced. A 404 from a backend is that backend's answer —
//! very often a JSON document an application depends on — and replacing it with
//! a page would break the client to make the browser look nicer. So the
//! directive acts on responses carrying the marker the gateway sets on its own
//! refusals: a route that matched nothing, a guard that said no, a backend that
//! could not be reached.
//!
//! Pages are read during prepare. A missing file stops the reload, rather than
//! producing a blank page the first time something goes wrong — which is
//! exactly the moment nobody is watching for a second problem.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use http::header::HeaderValue;
use salvo_core::http::{Request, ResBody, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// An error page is read into memory and kept there, so it is bounded to
/// something a person would actually write.
const MAX_PAGE: ByteSize = ByteSize(256 * 1024);

/// Which responses a page covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    Exact(u16),
    /// `4xx`, `5xx` — every status in that hundred.
    Class(u16),
}

impl Match {
    fn parse(text: &str) -> Option<Self> {
        if let Some(class) = text.strip_suffix("xx") {
            let class: u16 = class.parse().ok()?;
            return (1..=5).contains(&class).then_some(Match::Class(class));
        }
        let status: u16 = text.parse().ok()?;
        (100..=599).contains(&status).then_some(Match::Exact(status))
    }

    fn covers(self, status: u16) -> bool {
        match self {
            Match::Exact(exact) => exact == status,
            Match::Class(class) => status / 100 == class,
        }
    }

    fn describe(self) -> String {
        match self {
            Match::Exact(status) => status.to_string(),
            Match::Class(class) => format!("{class}xx"),
        }
    }

    /// How specific this is. An exact status wins over a class, whatever order
    /// they were written in.
    fn rank(self) -> u8 {
        match self {
            Match::Exact(_) => 0,
            Match::Class(_) => 1,
        }
    }
}

/// The status a page covers, written either way.
///
/// `page 404` and `page "5xx"` are both natural, and a decoder that accepted
/// only one of them would be a paper cut on every configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusText(String);

impl gatel_model::FromValue for StatusText {
    fn type_hint() -> &'static str {
        "a status such as 404, or a class such as \"5xx\""
    }

    fn from_value(value: &gatel_model::Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        if let Some(text) = value.as_str() {
            return Some(StatusText(text.to_string()));
        }
        if let Some(number) = value.as_int() {
            return Some(StatusText(number.to_string()));
        }
        diagnostics.error(
            Code::InvalidValue,
            value.origin.clone(),
            format!("expected {}", Self::type_hint()),
        );
        None
    }
}

#[derive(Debug, FromNode)]
struct PageNode {
    #[gatel(arg)]
    status: StatusText,
    /// A file read at prepare, relative to the configuration.
    #[gatel(prop)]
    file: Option<String>,
    /// Or the body written in place, for something short.
    #[gatel(prop)]
    body: Option<String>,
    #[gatel(prop)]
    content_type: Option<String>,
}

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(children, rename = "page")]
    pages: Vec<PageNode>,
    /// Act on every error, including one a backend produced.
    ///
    /// Off by default, because a backend's error body is usually the answer an
    /// application is waiting for.
    #[gatel(prop, default = false)]
    all: bool,
}

#[derive(Debug, Clone)]
struct Page {
    matcher: Match,
    source: Source,
    content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    File(String),
    Inline(String),
}

#[derive(Debug)]
pub struct Config {
    pages: Vec<Page>,
    all: bool,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_ordered(&self.pages, |page, hasher| {
            hasher.update_str(&page.matcher.describe());
            match &page.source {
                Source::File(path) => {
                    hasher.update_str("file");
                    hasher.update_str(path);
                }
                Source::Inline(body) => {
                    hasher.update_str("inline");
                    hasher.update_str(body);
                }
            }
            hasher.update_opt_str(page.content_type.as_deref());
        });
        hasher.update_bool(self.all);
    }

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        let mut pages = Vec::with_capacity(self.pages.len());
        for page in &self.pages {
            let (body, guessed) = match &page.source {
                Source::Inline(body) => (body.clone(), "text/plain; charset=utf-8"),
                Source::File(path) => {
                    let full = cx.root.join(path);
                    let size = std::fs::metadata(&full)
                        .map_err(|error| format!("cannot read `{path}`: {error}"))?
                        .len();
                    if size > MAX_PAGE.bytes() {
                        return Err(format!(
                            "`{path}` is {size} bytes; an error page is kept in memory and is limited to {MAX_PAGE}"
                        ));
                    }
                    let body = std::fs::read_to_string(&full)
                        .map_err(|error| format!("cannot read `{path}`: {error}"))?;
                    (body, guess(path))
                }
            };
            let content_type = page.content_type.as_deref().unwrap_or(guessed);
            pages.push(Prepared {
                matcher: page.matcher,
                body,
                content_type: HeaderValue::from_str(content_type)
                    .map_err(|_| format!("`{content_type}` cannot be sent as a content type"))?,
            });
        }
        Ok(Arc::new(ErrorPagesHandler { pages, all: self.all }))
    }

    fn describe(&self) -> String {
        let statuses: Vec<String> = self.pages.iter().map(|page| page.matcher.describe()).collect();
        let scope = if self.all { " (every error)" } else { "" };
        format!("error-pages {}{scope}", statuses.join(", "))
    }
}

fn guess(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        _ => "text/plain; charset=utf-8",
    }
}

pub struct ErrorPages;

impl Directive for ErrorPages {
    fn meta(&self) -> &DirectiveMeta {
        // The response phase: it changes what goes back, and only after
        // everything that could produce an error has run.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("error-pages", Phase::Response, Scopes::BOTH).with_version("2");
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.pages.is_empty() {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`error-pages` needs at least one `page`",
                )
                .with_help("`page 404 file=\"errors/404.html\"`"),
            );
            return None;
        }

        let mut pages = Vec::with_capacity(fields.pages.len());
        for page in fields.pages {
            let Some(matcher) = Match::parse(&page.status.0) else {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("`{}` is not a status or a class of statuses", page.status.0),
                    )
                    .with_help("a status such as `404`, or a class such as `5xx`"),
                );
                return None;
            };
            let source = match (page.file, page.body) {
                (Some(_), Some(_)) => {
                    diagnostics.error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("`page {}` has both a `file` and a `body`", page.status.0),
                    );
                    return None;
                }
                (Some(file), None) => Source::File(file),
                (None, Some(body)) => Source::Inline(body),
                (None, None) => {
                    diagnostics.push(
                        Diagnostic::error(
                            Code::MissingField,
                            node.origin.clone(),
                            format!("`page {}` needs a `file` or a `body`", page.status.0),
                        )
                        .with_help("`file=\"errors/404.html\"` or `body=\"Not found\\n\"`"),
                    );
                    return None;
                }
            };
            if pages.iter().any(|existing: &Page| existing.matcher == matcher) {
                diagnostics.error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("`{}` has two pages", page.status.0),
                );
                return None;
            }
            pages.push(Page {
                matcher,
                source,
                content_type: page.content_type,
            });
        }
        // Exact before class, so `404` wins over `4xx` whichever was written
        // first. Declaration order decides nothing here either.
        pages.sort_by_key(|page| (page.matcher.rank(), page.matcher.describe()));

        Some(Arc::new(Config {
            pages,
            all: fields.all,
        }))
    }
}

#[derive(Debug)]
struct Prepared {
    matcher: Match,
    body: String,
    content_type: HeaderValue,
}

struct ErrorPagesHandler {
    pages: Vec<Prepared>,
    all: bool,
}

#[async_trait]
impl Handler for ErrorPagesHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(request, depot, response).await;

        let status = response.status_code.map(|code| code.as_u16()).unwrap_or(200);
        if status < 400 {
            return;
        }
        // The marker the gateway sets on its own refusals. Without it, this is
        // a backend's answer and not ours to rewrite.
        if !self.all && !depot.get::<bool>("gatel.generated").copied().unwrap_or(false) {
            return;
        }
        let Some(page) = self.pages.iter().find(|page| page.matcher.covers(status)) else {
            return;
        };

        // These describe the body we are replacing, not the configured page.
        // In particular a zero-length 406 or an upstream encoded error must
        // not lend its framing, validators or digests to the new bytes.
        for name in [
            "content-length",
            "content-encoding",
            "content-range",
            "accept-ranges",
            "etag",
            "last-modified",
            "content-md5",
            "digest",
            "content-digest",
            "repr-digest",
            "trailer",
            "transfer-encoding",
            "content-location",
            "content-language",
            "content-disposition",
        ] {
            response.headers_mut().remove(name);
        }
        // Preserve status-specific controls (WWW-Authenticate, Retry-After,
        // Allow), cache policy and Vary. Only representation metadata is reset.
        *response.body_mut() = ResBody::None;
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, page.content_type.clone());
        response.headers_mut().insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_str(&page.body.len().to_string()).expect("bounded page length"),
        );
        let _ = response.write_body(page.body.clone());
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};
    use salvo_core::http::StatusCode;

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn page(status: &str, props: &[(&str, &str)]) -> Node {
        let mut node = Node::new("page", origin()).with_arg(Value::string(status, origin()));
        for (name, value) in props {
            node = node.with_prop(*name, Value::string(*value, origin()));
        }
        node
    }

    fn node(pages: Vec<Node>) -> Node {
        let mut node = Node::new("error-pages", origin());
        for page in pages {
            node = node.with_child(page);
        }
        node
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = ErrorPages.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn handler(node: Node) -> Arc<dyn Handler> {
        decode(node)
            .0
            .expect("the configuration is valid")
            .build(BuildContext {
                log_sinks: None,
                secrets: Default::default(),
                blocking: gatel_plan::blocking::Executor::process_default(),
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap()
    }

    /// A handler that answers with the status it was given.
    struct Failing(u16);

    #[async_trait]
    impl Handler for Failing {
        async fn handle(
            &self,
            _request: &mut Request,
            _depot: &mut Depot,
            response: &mut Response,
            _ctrl: &mut FlowCtrl,
        ) {
            response.status_code(StatusCode::from_u16(self.0).unwrap());
            let _ = response.write_body("the original body");
        }
    }

    async fn answer(pages: Arc<dyn Handler>, status: u16, generated: bool) -> (Response, String) {
        let mut depot = Depot::new();
        if generated {
            depot.insert("gatel.generated", true);
        }
        let mut response = Response::new();
        let mut ctrl = FlowCtrl::new(vec![pages, Arc::new(Failing(status))]);
        ctrl.call_next(&mut Request::default(), &mut depot, &mut response)
            .await;
        let body = match std::mem::take(response.body_mut()) {
            ResBody::Once(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            other => format!("{other:?}"),
        };
        (response, body)
    }

    #[tokio::test]
    async fn a_page_replaces_the_body_of_a_matching_error() {
        let pages = handler(node(vec![page("404", &[("body", "not here\n")])]));
        let (response, body) = answer(pages, 404, true).await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
        assert_eq!(body, "not here\n");
    }

    #[tokio::test]
    async fn a_backend_error_is_left_alone() {
        // Very often a JSON document an application is waiting for.
        let pages = handler(node(vec![page("404", &[("body", "not here\n")])]));
        let (_, body) = answer(pages, 404, false).await;
        assert_eq!(body, "the original body");
    }

    #[tokio::test]
    async fn every_error_can_be_covered_when_that_is_what_is_wanted() {
        let mut node = node(vec![page("404", &[("body", "not here\n")])]);
        node = node.with_prop("all", Value::bool(true, origin()));
        let (_, body) = answer(handler(node), 404, false).await;
        assert_eq!(body, "not here\n");
    }

    #[tokio::test]
    async fn a_class_covers_every_status_in_it() {
        let pages = handler(node(vec![page("5xx", &[("body", "something broke\n")])]));
        let (_, body) = answer(Arc::clone(&pages), 503, true).await;
        assert_eq!(body, "something broke\n");
        let (_, body) = answer(pages, 404, true).await;
        assert_eq!(body, "the original body", "a 4xx is not a 5xx");
    }

    #[tokio::test]
    async fn an_exact_status_wins_over_the_class_it_is_in() {
        // Written class-first, so only the ordering rule can make this pass.
        let pages = handler(node(vec![
            page("4xx", &[("body", "some client error\n")]),
            page("404", &[("body", "not here\n")]),
        ]));
        let (_, body) = answer(Arc::clone(&pages), 404, true).await;
        assert_eq!(body, "not here\n");
        let (_, body) = answer(pages, 403, true).await;
        assert_eq!(body, "some client error\n");
    }

    #[tokio::test]
    async fn a_successful_response_is_never_touched() {
        let pages = handler(node(vec![page("2xx", &[("body", "replaced\n")])]));
        let (_, body) = answer(pages, 200, true).await;
        assert_eq!(body, "the original body");
    }

    #[tokio::test]
    async fn the_content_type_follows_the_page() {
        let pages = handler(node(vec![page(
            "404",
            &[("body", "{}"), ("content-type", "application/json")],
        )]));
        let (response, _) = answer(pages, 404, true).await;
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn replacement_drops_old_representation_but_keeps_protocol_controls() {
        let pages = handler(node(vec![page("401", &[("body", "sign in")])]));
        let mut response = Response::new();
        let stale = [
            "content-length",
            "content-encoding",
            "content-range",
            "accept-ranges",
            "etag",
            "last-modified",
            "content-md5",
            "digest",
            "content-digest",
            "repr-digest",
            "trailer",
            "transfer-encoding",
            "content-location",
            "content-language",
            "content-disposition",
        ];
        for name in stale {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static("old"));
        }
        for (name, value) in [
            ("www-authenticate", "Basic realm=\"test\""),
            ("retry-after", "10"),
            ("allow", "GET, HEAD"),
            ("cache-control", "no-store"),
            ("vary", "accept-encoding"),
            ("set-cookie", "session=; Max-Age=0"),
        ] {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
        }
        let controls = response.headers().clone();
        let mut depot = Depot::new();
        crate::generated(&mut depot);
        let mut ctrl = FlowCtrl::new(vec![pages, Arc::new(Failing(401))]);
        ctrl.call_next(&mut Request::default(), &mut depot, &mut response)
            .await;
        for name in stale {
            if name == "content-length" {
                assert_eq!(response.headers()[name], "7");
            } else {
                assert!(!response.headers().contains_key(name), "{name}");
            }
        }
        for name in [
            "www-authenticate",
            "retry-after",
            "allow",
            "cache-control",
            "vary",
            "set-cookie",
        ] {
            assert_eq!(response.headers()[name], controls[name], "{name}");
        }
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        assert!(matches!(response.body_mut(), ResBody::Once(bytes) if bytes.as_ref() == b"sign in"));
    }

    #[test]
    fn a_page_needs_a_file_or_a_body() {
        let (config, diagnostics) = decode(node(vec![page("404", &[])]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("needs a `file` or a `body`"))
        );
    }

    #[test]
    fn a_page_cannot_have_both() {
        let (config, diagnostics) = decode(node(vec![page("404", &[("file", "a.html"), ("body", "b")])]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("both")));
    }

    #[test]
    fn a_status_may_be_written_as_a_number_or_a_string() {
        let numeric = Node::new("page", origin())
            .with_arg(Value::int(404, origin()))
            .with_prop("body", Value::string("x", origin()));
        let (config, diagnostics) = decode(node(vec![numeric]));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(config.unwrap().describe(), "error-pages 404");
    }

    #[test]
    fn a_status_that_is_not_one_is_refused() {
        for status in ["nope", "1000", "6xx", "99"] {
            let (config, _) = decode(node(vec![page(status, &[("body", "x")])]));
            assert!(config.is_none(), "{status} was accepted");
        }
    }

    #[test]
    fn two_pages_for_one_status_is_a_mistake_rather_than_a_race() {
        let (config, diagnostics) = decode(node(vec![
            page("404", &[("body", "a")]),
            page("404", &[("body", "b")]),
        ]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("two pages")));
    }

    #[test]
    fn error_pages_without_a_page_says_so() {
        let (config, diagnostics) = decode(Node::new("error-pages", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("at least one `page`"))
        );
    }

    #[test]
    fn a_missing_file_stops_the_reload() {
        let config = decode(node(vec![page("404", &[("file", "definitely-not-here.html")])]))
            .0
            .unwrap();
        let error = config
            .build(BuildContext {
                log_sinks: None,
                secrets: Default::default(),
                blocking: gatel_plan::blocking::Executor::process_default(),
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .err()
            .expect("a missing page must be found before it is needed");
        assert!(error.contains("cannot read"), "{error}");
    }

    #[test]
    fn a_content_type_is_guessed_from_the_extension() {
        assert_eq!(guess("errors/404.html"), "text/html; charset=utf-8");
        assert_eq!(guess("errors/404.json"), "application/json");
        assert_eq!(guess("errors/404"), "text/plain; charset=utf-8");
    }

    #[test]
    fn it_runs_in_the_response_phase() {
        assert_eq!(
            ErrorPages.meta().role,
            gatel_plan::Role::Middleware(Phase::Response)
        );
    }
}
