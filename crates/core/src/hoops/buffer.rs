use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Middleware that enforces request and response body size limits.
///
/// - When `max_request_body` is set: if the `Content-Length` header of the incoming request exceeds
///   the limit, a 413 Payload Too Large response is returned immediately without forwarding to the
///   upstream.
/// - When `max_response_body` is set: if the `Content-Length` header of the upstream response
///   exceeds the limit, a 502 Bad Gateway response is returned instead.
pub struct BufferLimitHoop {
    max_request_body: Option<usize>,
    max_response_body: Option<usize>,
}

impl BufferLimitHoop {
    pub fn new(max_request_body: Option<usize>, max_response_body: Option<usize>) -> Self {
        Self {
            max_request_body,
            max_response_body,
        }
    }
}

#[async_trait]
impl salvo::Handler for BufferLimitHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // Check the request body size via Content-Length header.
        if let Some(max_req) = self.max_request_body {
            if let Some(content_length) = req
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<usize>().ok())
            {
                if content_length > max_req {
                    debug!(
                        content_length,
                        limit = max_req,
                        "request body exceeds limit, returning 413"
                    );
                    res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
                    res.body("Payload Too Large");
                    ctrl.skip_rest();
                    return;
                }
            }
        }

        // Forward the request and check the response body size.
        ctrl.call_next(req, depot, res).await;

        if let Some(max_resp) = self.max_response_body {
            if let Some(content_length) = res
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<usize>().ok())
            {
                if content_length > max_resp {
                    debug!(
                        content_length,
                        limit = max_resp,
                        "response body exceeds limit, returning 502"
                    );
                    res.status_code(StatusCode::BAD_GATEWAY);
                    res.body("Bad Gateway: response body too large");
                }
            }
        }
    }
}
