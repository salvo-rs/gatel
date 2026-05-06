use http_body_util::{BodyExt, LengthLimitError, Limited};
use salvo::http::{ParseError, ReqBody, ResBody, StatusCode};
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Middleware that enforces request and response body size limits.
///
/// - When `max_request_body` is set: the incoming body is read with a hard byte limit before
///   forwarding downstream. Chunked or unknown-length bodies cannot bypass the limit.
/// - When `max_response_body` is set: the response body is read with a hard byte limit before
///   returning to the client. Chunked or unknown-length responses cannot bypass the limit.
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
        if let Some(max_req) = self.max_request_body {
            match req.payload_with_max_size(max_req).await {
                Ok(bytes) => {
                    let bytes = bytes.clone();
                    req.headers_mut()
                        .insert(http::header::CONTENT_LENGTH, bytes.len().into());
                    *req.body_mut() = ReqBody::Once(bytes);
                }
                Err(ParseError::PayloadTooLarge) => {
                    debug!(limit = max_req, "request body exceeds limit, returning 413");
                    res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
                    res.body("Payload Too Large");
                    ctrl.skip_rest();
                    return;
                }
                Err(error) => {
                    debug!(%error, "failed to read request body for buffer limit");
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.body("Bad Request");
                    ctrl.skip_rest();
                    return;
                }
            }
        }

        // Forward the request and check the response body size.
        ctrl.call_next(req, depot, res).await;

        if let Some(max_resp) = self.max_response_body {
            let body = res.take_body();
            match collect_res_body_limited(body, max_resp).await {
                Ok(body_bytes) => {
                    res.headers_mut()
                        .insert(http::header::CONTENT_LENGTH, body_bytes.len().into());
                    res.body(body_bytes);
                }
                Err(BodyLimitError::TooLarge) => {
                    debug!(
                        limit = max_resp,
                        "response body exceeds limit, returning 502"
                    );
                    res.status_code(StatusCode::BAD_GATEWAY);
                    res.body("Bad Gateway: response body too large");
                }
                Err(BodyLimitError::Read) => {
                    debug!("failed to read response body for buffer limit");
                    res.status_code(StatusCode::BAD_GATEWAY);
                    res.body("Bad Gateway: failed to read response body");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyLimitError {
    TooLarge,
    Read,
}

async fn collect_res_body_limited(
    body: ResBody,
    max_size: usize,
) -> Result<Vec<u8>, BodyLimitError> {
    if let ResBody::Once(bytes) = &body
        && bytes.len() > max_size
    {
        return Err(BodyLimitError::TooLarge);
    }

    let collected = Limited::new(body, max_size)
        .collect()
        .await
        .map_err(|error| {
            if error.is::<LengthLimitError>() {
                BodyLimitError::TooLarge
            } else {
                BodyLimitError::Read
            }
        })?;
    Ok(collected.to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[tokio::test]
    async fn collect_response_body_limited_accepts_body_within_limit() {
        let body = ResBody::Once(Bytes::from_static(b"hello"));

        let collected = collect_res_body_limited(body, 5).await.unwrap();

        assert_eq!(collected, b"hello");
    }

    #[tokio::test]
    async fn collect_response_body_limited_rejects_oversized_body() {
        let body = ResBody::Once(Bytes::from_static(b"hello"));

        let error = collect_res_body_limited(body, 4).await.unwrap_err();

        assert_eq!(error, BodyLimitError::TooLarge);
    }
}
