//! `forward-auth`: let another service decide whether this request proceeds.
//!
//! The pattern every identity proxy uses. Before the request reaches whatever
//! answers it, a second service is shown who is asking — the method, the
//! target, the host, the client address, and whichever of the caller's headers
//! carry identity. A 2xx means yes; anything else is the refusal, and the
//! refusal is what the client is shown, unchanged, so a redirect to a sign-in
//! page arrives as a redirect to a sign-in page.
//!
//! # What it does not do
//!
//! It never sends the request body to the authoriser, and never forwards the
//! authoriser's answer as though it were the route's. Both are deliberate: the
//! body may be enormous and is nobody else's business, and an authoriser that
//! could answer on the route's behalf would be a second terminal.
//!
//! # Why the upstream is declared rather than written inline
//!
//! `upstream="auth"` names the same kind of upstream a `proxy` uses, and the
//! reason is everything that comes with it: health checks, a connection pool,
//! TLS to the authoriser, timeouts, and balancing across more than one of them.
//! A private address written here would have none of that, and an
//! authorisation check that hangs is an outage of everything behind it.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node, ResourceId, ResourceKind};
use gatel_plan::{
    Answer, Ask, BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes, SharedUpstreams,
};
use salvo_core::http::{HeaderMap, Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// Headers passed on when the operator did not say which.
///
/// Both are where a browser and an API client put identity. Sending them by
/// default is what makes the directive work out of the box; sending anything
/// more by default would hand an authoriser headers it never asked for.
const DEFAULT_FORWARD: [&str; 2] = ["cookie", "authorization"];

/// Headers never copied out of an answer, whatever the operator listed.
///
/// They describe the connection to the authoriser and the body we already
/// dropped, so carrying them onto another request would describe something
/// that is not there.
const NEVER_COPY: [&str; 6] = [
    "connection",
    "content-length",
    "content-type",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
];

#[derive(Debug, FromNode)]
struct NameList {
    #[gatel(args)]
    names: Vec<String>,
}

#[derive(Debug, FromNode)]
struct Fields {
    /// The upstream to ask. Declared elsewhere, like a proxy's.
    #[gatel(prop)]
    upstream: String,
    /// The path on it that decides. Whatever it is, it is asked with `GET` and
    /// no body; the real method is in `X-Forwarded-Method`.
    #[gatel(prop, default = String::from("/"))]
    path: String,
    /// Which of the caller's headers the authoriser is shown.
    #[gatel(child, rename = "forward-headers")]
    forward: Option<NameList>,
    /// Which of the authoriser's response headers are added to the request
    /// before it continues — how an identity service tells the application who
    /// the caller is.
    #[gatel(child, rename = "copy-headers")]
    copy: Option<NameList>,
}

#[derive(Debug)]
pub struct Config {
    upstream: ResourceId,
    path: String,
    forward: Vec<String>,
    copy: Vec<String>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.upstream.name());
        hasher.update_str(&self.path);
        // Unordered: which headers travel is behaviour, the order they were
        // typed in is not.
        hasher.update_unordered(&self.forward, |name, hasher| {
            hasher.update_str(name);
        });
        hasher.update_unordered(&self.copy, |name, hasher| {
            hasher.update_str(name);
        });
    }

    fn upstreams(&self) -> Vec<ResourceId> {
        vec![self.upstream.clone()]
    }

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(ForwardAuthHandler {
            // Resolved once, here: a directive that looked its upstream up per
            // request could find a different one than the generation it belongs
            // to was compiled against.
            upstreams: cx.upstream(&self.upstream)?,
            upstream: self.upstream.clone(),
            path: self.path.clone(),
            forward: self.forward.clone(),
            copy: self.copy.clone(),
        }))
    }

    fn describe(&self) -> String {
        format!("forward-auth -> {}{}", self.upstream.name(), self.path)
    }
}

pub struct ForwardAuth;

impl Directive for ForwardAuth {
    fn meta(&self) -> &DirectiveMeta {
        // A guard, and a late one: rate limits and address filters are cheaper
        // than a round trip to another service, so they get to refuse first.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("forward-auth", Phase::Guard, Scopes::BOTH).with_order(50);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if !fields.path.starts_with('/') {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                format!("`path` must start with `/`, and `{}` does not", fields.path),
            );
            return None;
        }
        let names = |list: Option<NameList>, fallback: &[&str]| -> Vec<String> {
            list.map(|list| list.names)
                .unwrap_or_else(|| fallback.iter().map(|name| name.to_string()).collect())
                .into_iter()
                .map(|name| name.to_ascii_lowercase())
                .collect()
        };
        Some(Arc::new(Config {
            upstream: ResourceId::new(ResourceKind::Upstream, fields.upstream.as_str()),
            path: fields.path,
            forward: names(fields.forward, &DEFAULT_FORWARD),
            copy: names(fields.copy, &[]),
        }))
    }
}

struct ForwardAuthHandler {
    upstreams: SharedUpstreams,
    upstream: ResourceId,
    path: String,
    forward: Vec<String>,
    copy: Vec<String>,
}

impl ForwardAuthHandler {
    /// What the authoriser is told about the request it is deciding.
    fn question(&self, request: &Request, depot: &Depot) -> Ask {
        let mut headers = HeaderMap::new();
        for name in &self.forward {
            for value in request.headers().get_all(name.as_str()) {
                if let Ok(name) = salvo_core::http::HeaderName::from_bytes(name.as_bytes()) {
                    headers.append(name, value.clone());
                }
            }
        }
        // The request as it actually arrived. `X-Forwarded-Method` and
        // `-Uri` are how an authoriser tells "read this" from "delete this",
        // which it cannot do from a fixed verification path.
        let set = |headers: &mut HeaderMap, name: &'static str, value: &str| {
            if let Ok(value) = salvo_core::http::HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        };
        set(&mut headers, "x-forwarded-method", request.method().as_str());
        set(
            &mut headers,
            "x-forwarded-uri",
            request
                .uri()
                .path_and_query()
                .map(|value| value.as_str())
                .unwrap_or("/"),
        );
        set(
            &mut headers,
            "x-forwarded-proto",
            if depot.get::<bool>("gatel.tls").copied().unwrap_or(false) {
                "https"
            } else {
                "http"
            },
        );
        if let Some(host) = crate::authority(request) {
            set(&mut headers, "x-forwarded-host", host);
            // The authoriser is asked about the site the client named, not
            // about whichever backend happens to answer.
            set(&mut headers, "host", host);
        }
        // Written from what the dispatcher decided, never copied from the
        // client's own header: an authoriser that allowlists addresses must not
        // be handed one the caller chose.
        if let Ok(ip) = depot.get::<std::net::IpAddr>("gatel.client_ip") {
            set(&mut headers, "x-forwarded-for", &ip.to_string());
        }

        Ask {
            method: salvo_core::http::Method::GET,
            path: self.path.clone(),
            headers,
        }
    }

    /// Show the client exactly what the authoriser said.
    ///
    /// Not a refusal of our own: the difference between "sign in here" and
    /// "you may not" belongs to the service that knows, and rewriting it into
    /// a generic 403 would lose the only useful part.
    fn relay(&self, answer: Answer, response: &mut Response) {
        response.status_code(answer.status);
        for (name, value) in &answer.headers {
            if NEVER_COPY.contains(&name.as_str()) {
                continue;
            }
            response.headers_mut().append(name.clone(), value.clone());
        }
        if let Some(value) = answer.headers.get(salvo_core::http::header::CONTENT_TYPE) {
            response
                .headers_mut()
                .insert(salvo_core::http::header::CONTENT_TYPE, value.clone());
        }
        let _ = response.write_body(answer.body);
    }
}

#[async_trait]
impl Handler for ForwardAuthHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let ask = self.question(request, depot);
        let answer = match self.upstreams.ask(&self.upstream, ask).await {
            Ok(answer) => answer,
            Err(error) => {
                // An authoriser that cannot be reached is a refusal, not a
                // pass. The reason goes to the log, not to the client, because
                // the client cannot do anything with it and it names a backend.
                tracing::warn!(
                    target: "gatel::directive",
                    event = "forward_auth_unreachable",
                    upstream = self.upstream.name(),
                    detail = %error,
                );
                crate::generated(depot);
                response.status_code(StatusCode::SERVICE_UNAVAILABLE);
                let _ = response.write_body("503 the authorisation service could not be reached\n");
                ctrl.skip_rest();
                return;
            }
        };

        if !answer.agreed() {
            crate::generated(depot);
            self.relay(answer, response);
            ctrl.skip_rest();
            return;
        }

        // Agreed. Whatever the authoriser wants the application to know about
        // the caller is written onto the request, replacing anything the caller
        // sent under the same name — otherwise a client could name itself.
        for name in &self.copy {
            let Ok(header) = salvo_core::http::HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            request.headers_mut().remove(&header);
            for value in answer.headers.get_all(name.as_str()) {
                request.headers_mut().append(header.clone(), value.clone());
            }
        }
        ctrl.call_next(request, depot, response).await;
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};
    use gatel_plan::{HeaderMap, NoUpstreams, StatusCode as PlanStatus};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    /// `forward-auth upstream="auth" path=<path>`, with whatever child lists
    /// the test wants.
    fn node(path: &str, children: &[(&str, &[&str])]) -> Node {
        let mut node = Node::new("forward-auth", origin())
            .with_prop("upstream", Value::string("auth", origin()))
            .with_prop("path", Value::string(path, origin()));
        for (name, names) in children {
            let mut child = Node::new(*name, origin());
            for one in *names {
                child = child.with_arg(Value::string(*one, origin()));
            }
            node = node.with_child(child);
        }
        node
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = ForwardAuth.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn config(node: Node) -> Config {
        let mut diagnostics = Diagnostics::new();
        let fields = Fields::from_node(&node, &mut diagnostics).expect("valid fields");
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        Config {
            upstream: ResourceId::new(ResourceKind::Upstream, fields.upstream.as_str()),
            path: fields.path,
            forward: fields
                .forward
                .map(|list| list.names)
                .unwrap_or_else(|| DEFAULT_FORWARD.iter().map(|name| name.to_string()).collect()),
            copy: fields.copy.map(|list| list.names).unwrap_or_default(),
        }
    }

    fn handler(config: &Config) -> ForwardAuthHandler {
        ForwardAuthHandler {
            upstreams: Arc::new(NoUpstreams),
            upstream: config.upstream.clone(),
            path: config.path.clone(),
            forward: config.forward.clone(),
            copy: config.copy.clone(),
        }
    }

    #[test]
    fn identity_headers_travel_by_default() {
        // Without these the authoriser is asked about an anonymous request
        // every time, which it would rightly refuse.
        let config = config(node("/verify", &[]));
        assert_eq!(config.forward, ["cookie", "authorization"]);
        assert!(config.copy.is_empty());
    }

    #[test]
    fn a_path_that_is_not_a_path_is_refused_where_it_is_written() {
        let (config, diagnostics) = decode(node("verify", &[]));
        assert!(config.is_none());
        assert!(
            diagnostics.to_string().contains("must start with `/`"),
            "{diagnostics}"
        );
    }

    #[test]
    fn the_upstream_it_names_is_declared_so_compilation_can_check_it() {
        let (config, diagnostics) = decode(node("/verify", &[]));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        let named = config.unwrap().upstreams();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].name(), "auth");
    }

    #[test]
    fn the_question_says_what_the_request_was_rather_than_what_the_path_is() {
        let config = config(node("/verify", &[("forward-headers", &["cookie"])]));
        let handler = handler(&config);
        let mut request = Request::default();
        *request.method_mut() = salvo_core::http::Method::DELETE;
        *request.uri_mut() = "/orders/7?force=1".parse().unwrap();
        request.headers_mut().insert(
            "cookie",
            salvo_core::http::HeaderValue::from_static("session=abc"),
        );
        request.headers_mut().insert(
            "x-secret",
            salvo_core::http::HeaderValue::from_static("not yours"),
        );
        let mut depot = Depot::new();
        depot.insert("gatel.tls", true);

        let ask = handler.question(&request, &depot);
        assert_eq!(ask.path, "/verify");
        assert_eq!(ask.method, salvo_core::http::Method::GET);
        assert_eq!(ask.headers["x-forwarded-method"], "DELETE");
        assert_eq!(ask.headers["x-forwarded-uri"], "/orders/7?force=1");
        assert_eq!(ask.headers["x-forwarded-proto"], "https");
        assert_eq!(ask.headers["cookie"], "session=abc");
        // Everything else stays where it was: an authoriser is shown identity,
        // not the whole request.
        assert!(!ask.headers.contains_key("x-secret"));
    }

    #[test]
    fn a_client_cannot_write_its_own_forwarded_address() {
        let config = config(node("/verify", &[]));
        let handler = handler(&config);
        let mut request = Request::default();
        request.headers_mut().insert(
            "x-forwarded-for",
            salvo_core::http::HeaderValue::from_static("10.0.0.9"),
        );
        let mut depot = Depot::new();
        depot.insert("gatel.client_ip", std::net::IpAddr::from([203, 0, 113, 7]));

        let ask = handler.question(&request, &depot);
        assert_eq!(ask.headers["x-forwarded-for"], "203.0.113.7");
    }

    #[test]
    fn a_refusal_is_relayed_whole() {
        // A 302 to a sign-in page is the common case, and it is useless
        // without its `Location`.
        let config = config(node("/verify", &[]));
        let handler = handler(&config);
        let mut headers = HeaderMap::new();
        headers.insert(
            "location",
            salvo_core::http::HeaderValue::from_static("https://id.example/sign-in"),
        );
        headers.insert(
            "content-length",
            salvo_core::http::HeaderValue::from_static("999"),
        );
        let mut response = Response::new();
        handler.relay(
            Answer {
                status: PlanStatus::FOUND,
                headers,
                body: b"go here".to_vec(),
            },
            &mut response,
        );
        assert_eq!(response.status_code, Some(StatusCode::FOUND));
        assert_eq!(response.headers()["location"], "https://id.example/sign-in");
        // The length described the authoriser's connection, not this response.
        assert!(!response.headers().contains_key("content-length"));
    }
}
