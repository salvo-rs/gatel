use std::collections::HashMap;

use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

use crate::config::HeadersConfig;
use crate::placeholder::expand_placeholders;

/// Request/response header manipulation middleware.
///
/// Supports setting, adding, and deleting headers on both the request
/// (before forwarding) and the response (before returning to client).
/// Header values may contain placeholders that are expanded at runtime:
///   - `{client_ip}` — client socket address IP
///   - `{host}` — request Host header value
///   - `{method}` — HTTP method
///   - `{path}` — request URI path
///   - `{scheme}` — request URI scheme (http/https)
pub struct HeadersHoop {
    request_set: Vec<(String, String)>,
    response_set: Vec<(String, String)>,
    response_remove: Vec<String>,
}

impl HeadersHoop {
    pub fn new(cfg: &HeadersConfig) -> Self {
        let request_set: Vec<(String, String)> = cfg
            .request_set
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let response_set: Vec<(String, String)> = cfg
            .response_set
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let response_remove = cfg.response_remove.clone();
        Self {
            request_set,
            response_set,
            response_remove,
        }
    }
}

#[async_trait]
impl salvo::Handler for HeadersHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Build placeholder values from the request.
        let placeholders = build_placeholders(req);

        // Apply request header modifications.
        for (name, value_template) in &self.request_set {
            let expanded = expand_placeholders(value_template, &placeholders);
            if let (Ok(hn), Ok(hv)) = (
                name.parse::<http::header::HeaderName>(),
                expanded.parse::<http::header::HeaderValue>(),
            ) {
                debug!(header = %name, value = %expanded, "setting request header");
                req.headers_mut().insert(hn, hv);
            }
        }

        // Forward to next middleware / handler.
        ctrl.call_next(req, depot, res).await;

        // Apply response header removals.
        for name in &self.response_remove {
            if let Ok(hn) = name.parse::<http::header::HeaderName>() {
                res.headers_mut().remove(hn);
            }
        }

        // Apply response header sets.
        for (name, value_template) in &self.response_set {
            let expanded = expand_placeholders(value_template, &placeholders);
            if let (Ok(hn), Ok(hv)) = (
                name.parse::<http::header::HeaderName>(),
                expanded.parse::<http::header::HeaderValue>(),
            ) {
                debug!(header = %name, value = %expanded, "setting response header");
                res.headers_mut().insert(hn, hv);
            }
        }
    }
}

/// Build a map of placeholder names to their current values from the request.
fn build_placeholders(req: &Request) -> HashMap<&'static str, String> {
    let client_addr = super::client_addr(req);
    let mut m = HashMap::new();
    m.insert("client_ip", client_addr.ip().to_string());
    m.insert(
        "host",
        req.headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    m.insert("method", req.method().to_string());
    m.insert("path", req.uri().path().to_string());
    m.insert(
        "scheme",
        req.uri().scheme_str().unwrap_or("http").to_string(),
    );
    m
}
