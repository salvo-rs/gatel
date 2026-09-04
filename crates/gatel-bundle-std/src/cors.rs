//! `cors`: tell a browser which other origins may read this route.
//!
//! CORS is a browser mechanism, not an access control. A request from an origin
//! that is not allowed is served exactly as it would have been; what changes is
//! that the browser will not hand the answer to the page. Answering 403 instead
//! — a common mistake — breaks every non-browser client while protecting
//! nothing, because anything that is not a browser simply ignores the whole
//! protocol. Access control is `basic-auth` and `ip-filter`.
//!
//! A preflight is different. It is a question, not a request, so it is answered
//! here and never reaches the terminal.

use std::sync::Arc;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use http::header::{HeaderName, HeaderValue};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// What a browser may be told, when the configuration does not say.
const DEFAULT_METHODS: &str = "GET, HEAD, POST";

/// One allowed origin: `origin "https://app.example.com"`.
///
/// A child node rather than a comma-separated property, because an origin is a
/// thing rather than a list entry, and a list of them wants one per line.
#[derive(Debug, FromNode)]
struct OriginNode {
    #[gatel(arg)]
    value: String,
}

#[derive(Debug, FromNode)]
struct Fields {
    /// One or more allowed origins, or `*`.
    #[gatel(children, rename = "origin")]
    origins: Vec<OriginNode>,
    #[gatel(prop)]
    methods: Option<String>,
    /// Request headers a browser may send.
    #[gatel(prop)]
    headers: Option<String>,
    /// Response headers a page may read. Without this a page sees only the
    /// handful the standard calls safe.
    #[gatel(prop)]
    expose: Option<String>,
    /// Whether the browser may send cookies and credentials.
    #[gatel(prop, default = false)]
    credentials: bool,
    /// How long a preflight answer may be reused.
    #[gatel(prop)]
    max_age: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origins {
    /// Any origin, which the standard only allows without credentials.
    Any,
    Exact(Vec<String>),
}

impl Origins {
    fn allows(&self, origin: &str) -> bool {
        match self {
            Origins::Any => true,
            Origins::Exact(allowed) => allowed.iter().any(|known| known == origin),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    origins: Origins,
    methods: String,
    headers: Option<String>,
    expose: Option<String>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        match &self.origins {
            Origins::Any => {
                hasher.update_str("*");
            }
            Origins::Exact(origins) => {
                hasher.update_ordered(origins, |origin, hasher| {
                    hasher.update_str(origin);
                });
            }
        }
        hasher.update_str(&self.methods);
        hasher.update_opt_str(self.headers.as_deref());
        hasher.update_opt_str(self.expose.as_deref());
        hasher.update_bool(self.credentials);
        match self.max_age {
            Some(age) => {
                hasher.update_duration(age);
            }
            None => {
                hasher.update_none();
            }
        }
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        let header = |value: &str| {
            HeaderValue::from_str(value).map_err(|_| format!("`{value}` cannot be sent as a header"))
        };
        Ok(Arc::new(CorsHandler {
            origins: self.origins.clone(),
            methods: header(&self.methods)?,
            headers: self.headers.as_deref().map(header).transpose()?,
            expose: self.expose.as_deref().map(header).transpose()?,
            credentials: self.credentials,
            max_age: self
                .max_age
                .map(|age| header(&age.as_secs().to_string()))
                .transpose()?,
        }))
    }

    fn describe(&self) -> String {
        let origins = match &self.origins {
            Origins::Any => "any origin".to_string(),
            Origins::Exact(origins) => format!("{} origin(s)", origins.len()),
        };
        let credentials = if self.credentials {
            ", with credentials"
        } else {
            ""
        };
        format!("cors {origins}{credentials}")
    }
}

pub struct Cors;

impl Directive for Cors {
    fn meta(&self) -> &DirectiveMeta {
        // A guard, because a preflight is answered here and goes no further.
        static META: DirectiveMeta = DirectiveMeta::middleware("cors", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        let origins: Vec<String> = fields.origins.into_iter().map(|origin| origin.value).collect();
        if origins.is_empty() {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`cors` needs at least one `origin`",
                )
                .with_help("`origin \"https://app.example.com\"`, or `origin \"*\"` for any"),
            );
            return None;
        }

        let any = origins.iter().any(|origin| origin == "*");
        if any && origins.len() > 1 {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`origin \"*\"` already covers every origin",
                )
                .with_help("list the origins, or list only `*`"),
            );
            return None;
        }
        if any && fields.credentials {
            // Not a preference: a browser refuses this combination outright, so
            // accepting it would produce a configuration that never works.
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`credentials` cannot be used with `origin \"*\"`",
                )
                .with_help("name the origins that may send credentials"),
            );
            return None;
        }
        for origin in &origins {
            if origin != "*" && !origin.starts_with("http://") && !origin.starts_with("https://") {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("`{origin}` is not an origin"),
                    )
                    .with_help("an origin is a scheme, a host and an optional port, and nothing else"),
                );
                return None;
            }
            if origin.matches('/').count() > 2 {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("`{origin}` has a path; an origin does not"),
                    )
                    .with_help("a browser sends only the scheme, host and port"),
                );
                return None;
            }
        }

        Some(Arc::new(Config {
            origins: if any {
                Origins::Any
            } else {
                Origins::Exact(origins)
            },
            methods: fields
                .methods
                .map(|methods| normalise(&methods))
                .unwrap_or_else(|| DEFAULT_METHODS.to_string()),
            headers: fields.headers.as_deref().map(normalise),
            expose: fields.expose.as_deref().map(normalise),
            credentials: fields.credentials,
            max_age: fields.max_age,
        }))
    }
}

/// A comma-separated list, written the way a header carries it.
fn normalise(list: &str) -> String {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

struct CorsHandler {
    origins: Origins,
    methods: HeaderValue,
    headers: Option<HeaderValue>,
    expose: Option<HeaderValue>,
    credentials: bool,
    max_age: Option<HeaderValue>,
}

impl CorsHandler {
    /// What `Access-Control-Allow-Origin` should say, if anything.
    fn allow(&self, origin: &str) -> Option<HeaderValue> {
        if !self.origins.allows(origin) {
            return None;
        }
        match self.origins {
            // The literal star only when credentials are off, which decoding
            // already guarantees.
            Origins::Any if !self.credentials => HeaderValue::from_static("*").into(),
            _ => HeaderValue::from_str(origin).ok(),
        }
    }

    fn write_common(&self, response: &mut Response, allow: HeaderValue) {
        set(response, http::header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        if self.credentials {
            set(
                response,
                http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
    }
}

#[async_trait]
impl Handler for CorsHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let origin = request
            .headers()
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        // The answer differs by origin whenever more than one is allowed, so it
        // must not be cached as if it did not. Set even when the origin is
        // refused, because the refusal is itself origin-dependent.
        vary(response, "origin");

        let preflight = request.method() == http::Method::OPTIONS
            && request
                .headers()
                .contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD);

        let allow = origin.as_deref().and_then(|origin| self.allow(origin));

        if preflight {
            vary(response, "access-control-request-method");
            vary(response, "access-control-request-headers");
            if let Some(allow) = allow {
                self.write_common(response, allow);
                set(
                    response,
                    http::header::ACCESS_CONTROL_ALLOW_METHODS,
                    self.methods.clone(),
                );
                if let Some(headers) = &self.headers {
                    set(
                        response,
                        http::header::ACCESS_CONTROL_ALLOW_HEADERS,
                        headers.clone(),
                    );
                }
                if let Some(max_age) = &self.max_age {
                    set(response, http::header::ACCESS_CONTROL_MAX_AGE, max_age.clone());
                }
            }
            // Answered either way, and never passed on: a preflight is a
            // question about the route, not a request to it. An unanswered one
            // is refused by the browser, which is the whole mechanism.
            response.status_code(StatusCode::NO_CONTENT);
            ctrl.skip_rest();
            return;
        }

        ctrl.call_next(request, depot, response).await;

        if let Some(allow) = allow {
            self.write_common(response, allow);
            if let Some(expose) = &self.expose {
                set(
                    response,
                    http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
                    expose.clone(),
                );
            }
        }
    }
}

fn set(response: &mut Response, name: http::header::HeaderName, value: HeaderValue) {
    response.headers_mut().insert(name, value);
}

/// Add one name to `Vary` without losing what is already there.
fn vary(response: &mut Response, name: &'static str) {
    let existing = response
        .headers()
        .get(http::header::VARY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if existing
        .split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case(name))
    {
        return;
    }
    let combined = if existing.is_empty() {
        name.to_string()
    } else {
        format!("{existing}, {name}")
    };
    if let Ok(value) = HeaderValue::from_str(&combined) {
        response.headers_mut().insert(http::header::VARY, value);
    }
}

/// Kept so the header names used above are checked against `http`.
#[allow(dead_code)]
const CHECKED: [HeaderName; 1] = [http::header::ORIGIN];

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn child(name: &str, value: Value) -> Node {
        Node::new(name, origin()).with_arg(value)
    }

    fn node(origins: &[&str], extra: &[(&str, Value)]) -> Node {
        let mut node = Node::new("cors", origin());
        for value in origins {
            node = node.with_child(child("origin", Value::string(*value, origin())));
        }
        for (name, value) in extra {
            node = node.with_prop(*name, value.clone());
        }
        node
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = Cors.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn handler(node: Node) -> Arc<dyn Handler> {
        decode(node)
            .0
            .expect("the configuration is valid")
            .build(BuildContext {
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
            })
            .unwrap()
    }

    fn request(method: &str, headers: &[(&str, &str)]) -> Request {
        let mut request = Request::default();
        *request.method_mut() = method.parse().unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        request
    }

    async fn answer(handler: &dyn Handler, mut request: Request) -> Response {
        let mut response = Response::new();
        handler
            .handle(
                &mut request,
                &mut Depot::new(),
                &mut response,
                &mut FlowCtrl::new(Vec::new()),
            )
            .await;
        response
    }

    fn header(response: &Response, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    #[tokio::test]
    async fn an_allowed_origin_is_echoed_back() {
        let handler = handler(node(&["https://app.example.com"], &[]));
        let response = answer(
            handler.as_ref(),
            request("GET", &[("origin", "https://app.example.com")]),
        )
        .await;
        assert_eq!(
            header(&response, "access-control-allow-origin").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(header(&response, "vary").as_deref(), Some("origin"));
    }

    #[tokio::test]
    async fn an_origin_that_is_not_allowed_is_served_without_the_header() {
        let handler = handler(node(&["https://app.example.com"], &[]));
        let response = answer(
            handler.as_ref(),
            request("GET", &[("origin", "https://evil.example.com")]),
        )
        .await;
        // Served, not refused: CORS is a browser mechanism, and answering 403
        // would break every client that is not a browser while protecting
        // nothing.
        assert_eq!(response.status_code, None);
        assert_eq!(header(&response, "access-control-allow-origin"), None);
        assert_eq!(
            header(&response, "vary").as_deref(),
            Some("origin"),
            "the answer still depends on the origin"
        );
    }

    #[tokio::test]
    async fn a_star_origin_is_sent_literally() {
        let handler = handler(node(&["*"], &[]));
        let response = answer(
            handler.as_ref(),
            request("GET", &[("origin", "https://anything.example.com")]),
        )
        .await;
        assert_eq!(
            header(&response, "access-control-allow-origin").as_deref(),
            Some("*")
        );
    }

    #[tokio::test]
    async fn a_preflight_is_answered_here_and_goes_no_further() {
        let handler = handler(node(
            &["https://app.example.com"],
            &[
                ("methods", Value::string("GET,POST,DELETE", origin())),
                ("headers", Value::string("content-type, authorization", origin())),
                ("max-age", Value::string("1h", origin())),
            ],
        ));
        let response = answer(
            handler.as_ref(),
            request(
                "OPTIONS",
                &[
                    ("origin", "https://app.example.com"),
                    ("access-control-request-method", "DELETE"),
                ],
            ),
        )
        .await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(
            header(&response, "access-control-allow-methods").as_deref(),
            Some("GET, POST, DELETE")
        );
        assert_eq!(
            header(&response, "access-control-allow-headers").as_deref(),
            Some("content-type, authorization")
        );
        assert_eq!(
            header(&response, "access-control-max-age").as_deref(),
            Some("3600")
        );
        assert!(
            header(&response, "vary")
                .unwrap()
                .contains("access-control-request-method")
        );
    }

    #[tokio::test]
    async fn a_plain_options_request_is_not_a_preflight() {
        // Without `Access-Control-Request-Method` this is somebody asking what
        // the route supports, which is the route's business.
        let handler = handler(node(&["https://app.example.com"], &[]));
        let response = answer(
            handler.as_ref(),
            request("OPTIONS", &[("origin", "https://app.example.com")]),
        )
        .await;
        assert_ne!(response.status_code, Some(StatusCode::NO_CONTENT));
    }

    #[tokio::test]
    async fn a_refused_preflight_is_answered_without_the_permission() {
        let handler = handler(node(&["https://app.example.com"], &[]));
        let response = answer(
            handler.as_ref(),
            request(
                "OPTIONS",
                &[
                    ("origin", "https://evil.example.com"),
                    ("access-control-request-method", "DELETE"),
                ],
            ),
        )
        .await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(header(&response, "access-control-allow-methods"), None);
    }

    #[tokio::test]
    async fn credentials_are_announced_only_when_configured() {
        let with = handler(node(
            &["https://app.example.com"],
            &[("credentials", Value::bool(true, origin()))],
        ));
        let response = answer(
            with.as_ref(),
            request("GET", &[("origin", "https://app.example.com")]),
        )
        .await;
        assert_eq!(
            header(&response, "access-control-allow-credentials").as_deref(),
            Some("true")
        );

        let without = handler(node(&["https://app.example.com"], &[]));
        let response = answer(
            without.as_ref(),
            request("GET", &[("origin", "https://app.example.com")]),
        )
        .await;
        assert_eq!(header(&response, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn a_request_without_an_origin_is_left_alone() {
        let handler = handler(node(&["*"], &[]));
        let response = answer(handler.as_ref(), request("GET", &[])).await;
        assert_eq!(header(&response, "access-control-allow-origin"), None);
    }

    #[test]
    fn credentials_with_any_origin_is_refused_because_no_browser_accepts_it() {
        let (config, diagnostics) = decode(node(&["*"], &[("credentials", Value::bool(true, origin()))]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot be used with"))
        );
    }

    #[test]
    fn an_origin_with_a_path_is_refused() {
        let (config, diagnostics) = decode(node(&["https://app.example.com/api"], &[]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("has a path")));
    }

    #[test]
    fn something_that_is_not_an_origin_is_refused() {
        let (config, diagnostics) = decode(node(&["app.example.com"], &[]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("is not an origin")));
    }

    #[test]
    fn a_star_among_named_origins_is_a_mistake() {
        let (config, diagnostics) = decode(node(&["*", "https://app.example.com"], &[]));
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("already covers")));
    }

    #[test]
    fn cors_without_an_origin_says_so() {
        let (config, diagnostics) = decode(Node::new("cors", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("at least one `origin`"))
        );
    }

    #[test]
    fn a_list_is_written_the_way_a_header_carries_it() {
        assert_eq!(normalise("a,b ,, c"), "a, b, c");
    }

    #[test]
    fn it_is_a_guard_so_a_preflight_never_reaches_a_terminal() {
        assert_eq!(Cors.meta().role, gatel_plan::Role::Middleware(Phase::Guard));
    }
}
