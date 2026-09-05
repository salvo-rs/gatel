//! What the proxy must keep true regardless of the backend.
//!
//! Everything here is about a decision the proxy makes on its own: what it
//! forwards, what it refuses to forward, what it tells the client when it
//! cannot reach anything, and which failures it is allowed to try again.

use salvo_core::http::header::HeaderValue;

use super::outbound::{connection_tokens, cookie_value, has_body};
use super::*;

fn request_with(headers: &[(&str, &str)]) -> Request {
    let mut request = Request::default();
    for (name, value) in headers {
        request.headers_mut().append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    request
}

#[test]
fn a_request_without_a_body_may_be_retried() {
    assert!(!has_body(&request_with(&[])));
    assert!(!has_body(&request_with(&[("content-length", "0")])));
    assert!(has_body(&request_with(&[("content-length", "12")])));
    assert!(has_body(&request_with(&[("transfer-encoding", "chunked")])));
}

#[test]
fn headers_named_by_connection_are_treated_as_hop_by_hop() {
    let request = request_with(&[("connection", "keep-alive, X-Custom-Hop"), ("x-custom-hop", "1")]);
    let tokens = connection_tokens(&request);
    assert!(tokens.contains(&"x-custom-hop".to_string()));
    assert!(tokens.contains(&"keep-alive".to_string()));
}

#[test]
fn cookies_are_found_among_others() {
    let request = request_with(&[("cookie", "a=1; session=abc; b=2")]);
    assert_eq!(cookie_value(&request, "session"), Some("abc"));
    assert_eq!(cookie_value(&request, "absent"), None);
}

#[test]
fn failures_never_leak_the_backend_into_the_response() {
    let error = ProxyError::Upstream("connection refused by 10.0.0.7:9000".into());
    assert!(!error.public_message().contains("10.0.0.7"));
    assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
    // The detail is still available for the log.
    assert!(error.detail().contains("10.0.0.7"));
}

#[test]
fn capacity_problems_are_not_reported_as_bad_gateway() {
    let full = ProxyError::Checkout(CheckoutError::QueueFull { waiting: 9, limit: 8 });
    assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
    let refused = ProxyError::Checkout(CheckoutError::Connect("refused".into()));
    assert_eq!(refused.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        ProxyError::Timeout("response-head", Duration::from_secs(1)).status(),
        StatusCode::GATEWAY_TIMEOUT
    );
}
