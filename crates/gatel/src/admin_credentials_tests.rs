use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::*;
use crate::admin::tests::{TOKEN, setup};

const READ_TOKEN: &str = "fedcba9876543210fedcba9876543210";
struct NeverRead;
impl Body for NeverRead {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        panic!("credential rejection or diagnostic GET must not read body");
    }
}

#[tokio::test]
async fn read_credential_is_limited_to_exact_diagnostic_gets() {
    let (app, api) = setup().await;
    let api = api
        .with_read_token(Some(zeroize::Zeroizing::new(READ_TOKEN.into())))
        .unwrap();
    let before = app.configuration_status().version();
    for (method, path, expected) in [
        ("GET", "/v1/config/version", 200),
        ("GET", "/v1/config/status", 200),
        ("PUT", "/v1/config", 403),
        ("POST", "/v1/config/status", 403),
        ("GET", "/v1/config/status?x=1", 403),
        ("GET", "/v1/config", 403),
        ("GET", "/unknown", 403),
    ] {
        let response = api
            .handle(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {READ_TOKEN}"))
                    .header(header::IF_MATCH, before.etag())
                    .header(header::CONTENT_TYPE, "application/kdl")
                    .body(NeverRead)
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status().as_u16(), expected);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains(READ_TOKEN));
    }
    assert_eq!(app.configuration_status().version(), before);
    assert!(app.configuration_status().last_failure.is_none());
    let response = api
        .handle(
            Request::builder()
                .method("PUT")
                .uri("/v1/config")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::IF_MATCH, before.etag())
                .header(header::CONTENT_TYPE, "application/kdl")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    app.shutdown().await;
}

#[tokio::test]
async fn read_credential_must_be_valid_and_distinct_from_write_credential() {
    let (app, api) = setup().await;
    assert!(
        api.with_read_token(Some(zeroize::Zeroizing::new(TOKEN.into())))
            .is_err()
    );
    for token in ["short".to_owned(), " ".repeat(32), "x".repeat(257)] {
        let api = AdminApi::new(&app, zeroize::Zeroizing::new(TOKEN.into())).unwrap();
        assert!(api.with_read_token(Some(zeroize::Zeroizing::new(token))).is_err());
    }
    app.shutdown().await;
}
