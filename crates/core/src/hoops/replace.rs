use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Response body text-replacement middleware.
///
/// After the inner chain produces a response this middleware collects the body,
/// converts it to a UTF-8 string, applies each `(search, replacement)` rule in
/// order, updates the `Content-Length` header to reflect the new body size, and
/// returns the modified response.
///
/// Replacements are only applied when the response `Content-Type` matches one of
/// the configured MIME types (default: `["text/html"]`).
pub struct ReplaceHoop {
    rules: Vec<(String, String)>,
    once: bool,
    content_types: Vec<String>,
}

impl ReplaceHoop {
    pub fn new(rules: Vec<(String, String)>, once: bool) -> Self {
        Self {
            rules,
            once,
            content_types: vec!["text/html".to_string()],
        }
    }

    /// Return true if the response Content-Type header matches any of the
    /// configured MIME types.
    fn content_type_matches(&self, headers: &http::HeaderMap) -> bool {
        let ct = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        self.content_types
            .iter()
            .any(|allowed| ct.contains(allowed.as_str()))
    }
}

#[async_trait]
impl salvo::Handler for ReplaceHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;

        // Only modify responses with matching content types.
        if !self.content_type_matches(res.headers()) {
            return;
        }

        // Take the body and collect it.
        let body = res.take_body();
        let body_bytes = match super::compress::collect_res_body_bytes(body).await {
            Ok(b) => b,
            Err(_) => return,
        };

        // Convert to string; skip replacement on non-UTF-8 bodies.
        let text = match std::str::from_utf8(&body_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => {
                debug!("response body is not valid UTF-8; skipping replace middleware");
                res.body(body_bytes);
                return;
            }
        };

        // Apply each rule.
        let mut output = text;
        for (search, replacement) in &self.rules {
            output = if self.once {
                output.replacen(search.as_str(), replacement.as_str(), 1)
            } else {
                output.replace(search.as_str(), replacement.as_str())
            };
            debug!(
                search = search.as_str(),
                replacement = replacement.as_str(),
                once = self.once,
                "applied body replacement rule"
            );
        }

        // Rebuild with the updated body and Content-Length.
        let new_bytes = output.into_bytes();
        res.headers_mut()
            .insert(CONTENT_LENGTH, http::HeaderValue::from(new_bytes.len()));
        res.body(new_bytes);
    }
}
