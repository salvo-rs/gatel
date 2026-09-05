//! `force-https`: answer a plaintext request with a redirect to the secure one.
//!
//! A guard rather than a terminal, so it can sit on a site and cover every
//! route under it, and so it runs before anything that would look at the
//! request body or reach a backend.

use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use http::header::HeaderValue;
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, FromNode)]
struct Fields {
    /// The port the secure listener is on, when it is not 443.
    #[gatel(prop)]
    port: Option<u16>,
    /// 307 rather than 308, for a service that is not sure yet.
    #[gatel(prop, default = true)]
    permanent: bool,
}

#[derive(Debug)]
pub struct Config {
    port: Option<u16>,
    status: StatusCode,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        match self.port {
            Some(port) => {
                hasher.update_u64(port as u64);
            }
            None => {
                hasher.update_none();
            }
        }
        hasher.update_u64(self.status.as_u16() as u64);
    }

    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(ForceHttpsHandler {
            port: self.port,
            status: self.status,
        }))
    }

    fn describe(&self) -> String {
        match self.port {
            Some(port) => format!("force-https {} -> :{port}", self.status.as_u16()),
            None => format!("force-https {}", self.status.as_u16()),
        }
    }
}

pub struct ForceHttps;

impl Directive for ForceHttps {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("force-https", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.port == Some(0) {
            diagnostics.error(Code::InvalidValue, node.origin.clone(), "port 0 is not a port");
            return None;
        }
        Some(Arc::new(Config {
            port: fields.port,
            // The method and body survive, which is what someone submitting a
            // form over plaintext needs; 301 would turn their POST into a GET.
            status: if fields.permanent {
                StatusCode::PERMANENT_REDIRECT
            } else {
                StatusCode::TEMPORARY_REDIRECT
            },
        }))
    }
}

struct ForceHttpsHandler {
    port: Option<u16>,
    status: StatusCode,
}

#[async_trait]
impl Handler for ForceHttpsHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Whether this connection is secure is decided by the listener, not by
        // a header a client can write.
        if depot.get::<bool>("gatel.tls").copied().unwrap_or(false) {
            ctrl.call_next(request, depot, response).await;
            return;
        }
        let Some(location) = self.location(request) else {
            // No host, so nowhere to send them. Better a plain refusal than a
            // redirect to somewhere invented.
            crate::generated(depot);
            response.status_code(StatusCode::BAD_REQUEST);
            let _ = response.write_body("400 no host to redirect to\n");
            ctrl.skip_rest();
            return;
        };
        response.status_code(self.status);
        response.headers_mut().insert(http::header::LOCATION, location);
        ctrl.skip_rest();
    }
}

impl ForceHttpsHandler {
    fn location(&self, request: &Request) -> Option<HeaderValue> {
        let authority = crate::authority(request)?;
        let host = authority.split(':').next().unwrap_or(authority);
        let target = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let url = match self.port {
            Some(443) | None => format!("https://{host}{target}"),
            Some(port) => format!("https://{host}:{port}{target}"),
        };
        HeaderValue::from_str(&url).ok()
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn handler(props: &[(&str, Value)]) -> Arc<dyn Handler> {
        let mut node = Node::new("force-https", origin());
        for (name, value) in props {
            node = node.with_prop(*name, value.clone());
        }
        let mut diagnostics = Diagnostics::new();
        ForceHttps
            .decode(&node, &mut diagnostics)
            .expect("the configuration is valid")
            .build(BuildContext {
                root: std::path::Path::new("."),
                state: None,
                telemetry: crate::testing::telemetry(),
                upstreams: crate::testing::no_upstreams(),
            })
            .unwrap()
    }

    fn request(host: &str, target: &str) -> Request {
        let mut request = Request::default();
        *request.uri_mut() = target.parse().unwrap();
        if !host.is_empty() {
            request
                .headers_mut()
                .insert(http::header::HOST, HeaderValue::from_str(host).unwrap());
        }
        request
    }

    async fn answer(handler: &dyn Handler, mut request: Request, tls: bool) -> Response {
        let mut depot = Depot::new();
        depot.insert("gatel.tls", tls);
        let mut response = Response::new();
        handler
            .handle(
                &mut request,
                &mut depot,
                &mut response,
                &mut FlowCtrl::new(Vec::new()),
            )
            .await;
        response
    }

    fn location(response: &Response) -> Option<String> {
        response
            .headers()
            .get(http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    #[tokio::test]
    async fn a_plaintext_request_is_sent_to_the_secure_url() {
        let response = answer(handler(&[]).as_ref(), request("example.com", "/a/b?c=d"), false).await;
        assert_eq!(response.status_code, Some(StatusCode::PERMANENT_REDIRECT));
        assert_eq!(
            location(&response).as_deref(),
            Some("https://example.com/a/b?c=d"),
            "the path and query come along"
        );
    }

    #[tokio::test]
    async fn a_secure_request_is_left_alone() {
        let response = answer(handler(&[]).as_ref(), request("example.com", "/"), true).await;
        assert_eq!(response.status_code, None);
        assert_eq!(location(&response), None);
    }

    #[tokio::test]
    async fn a_non_standard_port_is_written_into_the_url() {
        let response = answer(
            handler(&[("port", Value::int(8443, origin()))]).as_ref(),
            request("example.com:8080", "/"),
            false,
        )
        .await;
        assert_eq!(
            location(&response).as_deref(),
            Some("https://example.com:8443/"),
            "the plaintext port must not be carried over"
        );
    }

    #[tokio::test]
    async fn the_default_port_is_left_out() {
        let response = answer(
            handler(&[("port", Value::int(443, origin()))]).as_ref(),
            request("example.com", "/"),
            false,
        )
        .await;
        assert_eq!(location(&response).as_deref(), Some("https://example.com/"));
    }

    #[tokio::test]
    async fn a_temporary_redirect_can_be_asked_for() {
        let response = answer(
            handler(&[("permanent", Value::bool(false, origin()))]).as_ref(),
            request("example.com", "/"),
            false,
        )
        .await;
        // A wrong permanent redirect is cached by browsers and effectively
        // cannot be taken back.
        assert_eq!(response.status_code, Some(StatusCode::TEMPORARY_REDIRECT));
    }

    #[tokio::test]
    async fn a_request_with_no_host_is_refused_rather_than_guessed_at() {
        let response = answer(handler(&[]).as_ref(), request("", "/"), false).await;
        assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
        assert_eq!(location(&response), None);
    }

    #[test]
    fn port_zero_is_refused() {
        let node = Node::new("force-https", origin()).with_prop("port", Value::int(0, origin()));
        let mut diagnostics = Diagnostics::new();
        assert!(ForceHttps.decode(&node, &mut diagnostics).is_none());
    }

    #[test]
    fn it_is_a_guard_so_it_runs_before_anything_reads_a_body() {
        assert_eq!(ForceHttps.meta().role, gatel_plan::Role::Middleware(Phase::Guard));
    }
}
