pub mod file_server;
pub mod redirect;

use http::Request;

use crate::{Body, ProxyError};

// ---------------------------------------------------------------------------
// Salvo/hyper conversion helpers
// ---------------------------------------------------------------------------

/// Convert a Salvo Request to a hyper `Request<Body>`.
pub fn strip_request(req: &mut salvo::Request) -> Result<Request<Body>, ProxyError> {
    use http_body_util::BodyExt;
    use salvo::http::ReqBody;

    let hyper_request = req
        .strip_to_hyper::<ReqBody>()
        .map_err(|error| ProxyError::Internal(format!("failed to convert request: {error}")))?;
    Ok(hyper_request.map(|body| {
        body.map_err(|error| -> crate::BoxError { Box::new(error) })
            .boxed()
    }))
}

/// Merge a hyper `Response<Body>` into a Salvo Response.
pub fn merge_response(res: &mut salvo::Response, response: http::Response<Body>) {
    use salvo::http::ResBody;

    let (parts, body) = response.into_parts();
    let response = hyper::Response::from_parts(parts, ResBody::Boxed(Box::pin(body)));
    res.merge_hyper(response);
}
