use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Forwarded authentication middleware.
///
/// Delegates authentication to an external service. For every incoming
/// request the middleware:
///   1. Clones the request headers (no body) and adds `X-Forwarded-Uri` (original path+query) and
///      `X-Forwarded-Method` (original method).
///   2. Sends a GET request to `auth_url` with those headers via reqwest.
///   3. If the auth service returns 2xx, copies the configured headers from the auth response into
///      the original request and continues the chain.
///   4. If the auth service returns non-2xx, that response is returned directly to the client.
pub struct ForwardAuthHoop {
    auth_url: String,
    copy_headers: Vec<String>,
    client: reqwest::Client,
}

impl ForwardAuthHoop {
    pub fn new(auth_url: String, copy_headers: Vec<String>) -> Self {
        Self {
            auth_url,
            copy_headers,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl salvo::Handler for ForwardAuthHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Build the forwarded-auth request: copy all incoming headers and
        // add X-Forwarded-Uri / X-Forwarded-Method.
        let forwarded_uri = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        let forwarded_method = req.method().to_string();

        let mut auth_req = self.client.get(&self.auth_url);

        // Copy all incoming request headers to the auth request.
        for (name, value) in req.headers() {
            if let Ok(val_str) = value.to_str() {
                auth_req = auth_req.header(name.as_str(), val_str);
            }
        }

        // Append the forwarded metadata headers.
        auth_req = auth_req
            .header("X-Forwarded-Uri", &forwarded_uri)
            .header("X-Forwarded-Method", &forwarded_method);

        let auth_resp = match auth_req.send().await {
            Ok(r) => r,
            Err(e) => {
                debug!(error = %e, "forward-auth request failed");
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.body(format!("forward-auth request failed: {e}"));
                ctrl.skip_rest();
                return;
            }
        };

        let auth_status = auth_resp.status();

        if auth_status.is_success() {
            // Copy the configured headers from the auth response into the
            // original request so downstream handlers can use them.
            for header_name in &self.copy_headers {
                if let Some(value) = auth_resp.headers().get(header_name.as_str()) {
                    if let Ok(hn) = header_name.parse::<http::header::HeaderName>() {
                        if let Ok(hv) = http::header::HeaderValue::from_bytes(value.as_bytes()) {
                            req.headers_mut().insert(hn, hv);
                        }
                    }
                }
            }

            debug!(auth_url = %self.auth_url, "forward-auth passed, continuing chain");
            ctrl.call_next(req, depot, res).await;
        } else {
            // Return the auth service's response to the client.
            debug!(
                auth_url = %self.auth_url,
                status = %auth_status,
                "forward-auth denied, returning auth response"
            );

            let status =
                StatusCode::from_u16(auth_status.as_u16()).unwrap_or(StatusCode::UNAUTHORIZED);

            // Save auth response headers before consuming the body.
            let auth_resp_headers = auth_resp.headers().clone();

            // Collect the auth response body.
            let body_bytes = auth_resp.bytes().await.unwrap_or_default();

            res.status_code(status);

            // Forward response headers from the auth service.
            // Skip transfer-encoding as it no longer applies to this response.
            for (name, value) in &auth_resp_headers {
                if name.as_str().eq_ignore_ascii_case("transfer-encoding") {
                    continue;
                }
                if let Ok(hv) = http::header::HeaderValue::from_bytes(value.as_bytes()) {
                    res.headers_mut().insert(name.clone(), hv);
                }
            }

            res.body(body_bytes.to_vec());
            ctrl.skip_rest();
        }
    }
}
