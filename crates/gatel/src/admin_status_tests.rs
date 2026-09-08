use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::*;
use crate::admin::tests::{TOKEN, setup};

struct NeverRead;
impl Body for NeverRead {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        panic!("status queries must not read a request body");
    }
}

#[tokio::test]
async fn unpublished_status_does_not_claim_a_serving_configuration() {
    let app = Arc::new(App::new_with_admin("status-unread.kdl".into(), vec![]).unwrap());
    let api = AdminApi::new(&app, zeroize::Zeroizing::new(TOKEN.into())).unwrap();
    let (_, status) = get(&api).await;
    assert_eq!(status["generation"], "0");
    assert_eq!(status["last_activated_generation"], "0");
    assert_eq!(status["published"], false);
    assert_eq!(status["observed_differs"], false);
    assert_eq!(status["health"]["ready"], false);
    assert_eq!(status["health"]["reasons"], serde_json::json!(["starting"]));
    app.shutdown().await;
}

async fn get(api: &AdminApi) -> (String, serde_json::Value) {
    let response = api
        .handle(
            Request::builder()
                .uri("/v1/config/status")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(NeverRead)
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert!(
        !response.headers().contains_key(header::ETAG),
        "configuration version is not a health response validator"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.len() < 2048);
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(!text.contains(TOKEN));
    assert!(!text.contains("private-secret"));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (value["configuration_etag"].as_str().unwrap().to_owned(), value)
}

#[tokio::test]
async fn status_is_redacted_and_distinguishes_rejection_recovery_and_local_failure() {
    let (app, api) = setup().await;
    let (tag, first) = get(&api).await;
    assert_eq!(first["configuration_etag"], tag);
    assert_eq!(first["generation"], "1");
    assert_eq!(first["last_activated_generation"], "1");
    assert_eq!(first["published"], true);
    assert_eq!(first["write_mode"], "memory");
    assert_eq!(first["observed_differs"], false);
    assert_eq!(first["health"]["state"], "ready");
    assert!(first["last_failure"].is_null());
    let writer = app.source_writer(SourceId::admin()).unwrap();
    let version = app.configuration_status().version();
    assert!(matches!(
        writer
            .reload_kdl_if(version, "invalid-directive \"private-secret\";".into())
            .await,
        Ok(Outcome::Rejected(_))
    ));
    let (next, rejected) = get(&api).await;
    assert_ne!(tag, next);
    assert_eq!(rejected["last_failure"]["stage"], "normalize");
    assert_eq!(rejected["last_failure"]["attempts"], 1);
    assert_eq!(rejected["observed_differs"], true);
    assert_eq!(rejected["health"]["state"], "degraded");
    assert_eq!(rejected["health"]["ready"], true);
    writer
        .reload_kdl_if(app.configuration_status().version(), String::new())
        .await
        .unwrap();
    assert!(get(&api).await.1["last_failure"].is_null());
    app.telemetry.health.persistence_unknown();
    app.telemetry.health.upstream("private-secret", false);
    app.telemetry.health.upstream("another-private-secret", false);
    let (_, failed) = get(&api).await;
    assert_eq!(failed["persistence_unknown"], true);
    assert_eq!(failed["published"], true);
    assert_eq!(failed["health"]["state"], "not_ready");
    assert_eq!(
        failed["health"]["reasons"],
        serde_json::json!(["persistence_unknown", "upstream_down"])
    );
    app.shutdown().await;
}

#[tokio::test]
async fn status_auth_origin_capacity_and_method_guards_do_not_read_body() {
    let (app, api) = setup().await;
    for (method, path, auth, origin, expected) in [
        ("GET", "/v1/config/status", false, false, 401),
        ("GET", "/v1/config/status", true, true, 403),
        ("POST", "/v1/config/status", true, false, 405),
        ("GET", "/v1/config/status?x=1", true, false, 404),
    ] {
        let mut request = Request::builder().method(method).uri(path);
        if auth {
            request = request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        }
        if origin {
            request = request.header(header::ORIGIN, "null");
        }
        let response = api.handle(request.body(NeverRead).unwrap()).await;
        assert_eq!(response.status().as_u16(), expected);
        assert!(!response.headers().contains_key(header::ETAG));
        if expected == 405 {
            assert_eq!(response.headers()[header::ALLOW], "GET");
        }
    }
    let slots = api.read_requests.acquire_many(2).await.unwrap();
    let request = || {
        Request::builder()
            .uri("/v1/config/status")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(NeverRead)
            .unwrap()
    };
    assert_eq!(
        api.handle(request()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(slots);
    app.shutdown().await;
    assert_eq!(
        api.handle(request()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}
