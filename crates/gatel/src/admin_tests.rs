use std::collections::VecDeque;
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use gatel_model::{Snapshot, SourceSnapshot};
use http_body::Frame;

use super::*;

pub(super) const TOKEN: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn persistence_unknown_is_not_reported_as_unapplied_or_retryable_conflict() {
    let response = write_reply(Err(SourceWriteError::PersistenceUnknown));
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(!response.headers().contains_key(header::ETAG));
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        &body[..],
        b"configuration may be applied; persistence outcome unknown; inspect before retrying"
    );
    let unavailable = write_reply(Err(SourceWriteError::Unavailable));
    assert_ne!(unavailable.into_body().collect().await.unwrap().to_bytes(), body);
    assert!(
        SourceWriteError::PersistenceUnknown
            .to_string()
            .contains("memory may already be updated")
    );
}

pub(super) async fn setup() -> (Arc<App>, AdminApi) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let config = format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"admin-base\"; }} }}"
    );
    let mut hash = Hasher::new();
    hash.update_str(&config);
    let snapshot = Snapshot::new()
        .with(
            SourceSnapshot::observe(
                SourceId::file("main"),
                hash.finish(),
                gatel_kdl::lower(&config, "admin-test").unwrap(),
                None,
            )
            .unwrap(),
        )
        .with(SourceSnapshot::observe(SourceId::admin(), Hasher::new().finish(), vec![], None).unwrap());
    let app = Arc::new(App::from_snapshot(std::env::temp_dir(), snapshot).unwrap());
    app.binds.adopt(socket).unwrap();
    assert!(matches!(app.reload().await, Outcome::Started { .. }));
    let api = AdminApi::new(&app, zeroize::Zeroizing::new(TOKEN.into())).unwrap();
    (app, api)
}

fn put<B>(version: &str, body: B) -> Request<B> {
    Request::builder()
        .method("PUT")
        .uri("/v1/config")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::IF_MATCH, version)
        .header(header::CONTENT_TYPE, "application/kdl")
        .body(body)
        .unwrap()
}

struct NeverRead;
impl Body for NeverRead {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        panic!("rejected request body must not be read")
    }
}

#[tokio::test]
async fn authentication_preconditions_and_capacity_precede_body_reads() {
    let (app, api) = setup().await;
    let version = app.configuration_status().version();
    let tag = version.etag();
    for length in ["+1", " 1", "1, 1"] {
        let mut request = put(&tag, NeverRead);
        request
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length.parse().unwrap());
        assert_eq!(api.handle(request).await.status(), StatusCode::BAD_REQUEST);
    }
    let mut ambiguous = put(&tag, NeverRead);
    ambiguous
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "1".parse().unwrap());
    ambiguous
        .headers_mut()
        .insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
    assert_eq!(api.handle(ambiguous).await.status(), StatusCode::BAD_REQUEST);
    for (mode, status) in [
        (0, 401),
        (1, 401),
        (2, 428),
        (3, 412),
        (4, 415),
        (5, 403),
        (6, 413),
    ] {
        let mut request = put(&tag, NeverRead);
        match mode {
            0 => {
                request.headers_mut().remove(header::AUTHORIZATION);
            }
            1 => {
                request
                    .headers_mut()
                    .append(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
            }
            2 => {
                request.headers_mut().remove(header::IF_MATCH);
            }
            3 => {
                request
                    .headers_mut()
                    .insert(header::IF_MATCH, "*".parse().unwrap());
            }
            4 => {
                request
                    .headers_mut()
                    .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
            }
            5 => {
                request
                    .headers_mut()
                    .insert(header::ORIGIN, "https://example.com".parse().unwrap());
            }
            _ => {
                request.headers_mut().insert(
                    header::CONTENT_LENGTH,
                    (MAX_BODY + 1).to_string().parse().unwrap(),
                );
            }
        }
        let result = api.handle(request).await;
        assert_eq!(result.status().as_u16(), status);
        assert_eq!(result.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!result.headers().contains_key("x-gatel-observed-differs"));
    }
    let slots = api.requests.acquire_many(8).await.unwrap();
    assert_eq!(
        api.handle(put(&tag, NeverRead)).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(slots);
    assert_eq!(app.configuration_status().version(), version);
    assert!(app.configuration_status().last_failure.is_none());
    app.shutdown().await;
}

#[tokio::test]
async fn authenticated_version_put_and_conflicts_use_normal_transactions() {
    let (app, api) = setup().await;
    let request = Request::builder()
        .uri("/v1/config/version")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(NeverRead)
        .unwrap();
    let response = api.handle(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gatel-observed-differs"], "false");
    let tag = response.headers()[header::ETAG].to_str().unwrap();
    let nodes = "upstream \"runtime/api\" { endpoint \"127.0.0.1:2\"; }";
    let response = api
        .handle(put(tag, Full::new(Bytes::from_static(nodes.as_bytes()))))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::ETAG));
    assert_eq!(app.published_for_test().unwrap().plan.upstreams.len(), 1);
    assert_eq!(
        api.handle(put(tag, NeverRead)).await.status(),
        StatusCode::PRECONDITION_FAILED
    );
    let tag = app.configuration_status().version().etag();
    let current = app.published_for_test().unwrap();
    let response = api
        .handle(put(
            &tag,
            Full::new(Bytes::from_static(b"secret-content-invalid {")),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"configuration rejected"));
    assert!(Arc::ptr_eq(&current, &app.published_for_test().unwrap()));
    assert_eq!(
        api.handle(put(&tag, Full::new(Bytes::from_static(nodes.as_bytes()))))
            .await
            .status(),
        StatusCode::OK
    );
    assert!(app.configuration_status().last_failure.is_none());
    app.shutdown().await;
    assert_eq!(
        api.handle(put(&tag, NeverRead)).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

struct Frames(VecDeque<Frame<Bytes>>);
impl Body for Frames {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}

#[tokio::test]
async fn streaming_limits_trailers_and_encoding_errors_do_not_observe_intent() {
    let (app, api) = setup().await;
    let version = app.configuration_status().version();
    let tag = version.etag();
    let frames = Frames(VecDeque::from([
        Frame::data(Bytes::from(vec![b' '; MAX_BODY])),
        Frame::data(Bytes::from_static(b" ")),
    ]));
    assert_eq!(
        api.handle(put(&tag, frames)).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let frames = Frames(VecDeque::from([Frame::trailers(http::HeaderMap::new())]));
    assert_eq!(
        api.handle(put(&tag, frames)).await.status(),
        StatusCode::BAD_REQUEST
    );
    let frames = Frames((0..=MAX_FRAMES).map(|_| Frame::data(Bytes::new())).collect());
    assert_eq!(
        api.handle(put(&tag, frames)).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        api.handle(put(&tag, Full::new(Bytes::from_static(&[255]))))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut request = put(&tag, Full::new(Bytes::new()));
    request
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "1".parse().unwrap());
    assert_eq!(api.handle(request).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.configuration_status().version(), version);
    assert!(app.configuration_status().last_failure.is_none());
    app.shutdown().await;
}

struct PendingBody;

struct ChannelBody(tokio::sync::mpsc::Receiver<Frame<Bytes>>);
impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

#[tokio::test]
async fn update_while_another_body_is_streaming_cannot_be_overwritten() {
    use std::future::Future;
    let (app, api) = setup().await;
    let version = app.configuration_status().version().etag();
    let (send, receive) = tokio::sync::mpsc::channel(1);
    let mut first = Box::pin(api.handle(put(&version, ChannelBody(receive))));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    let text = b"upstream \"runtime/winner\" { endpoint \"127.0.0.1:2\"; }";
    assert_eq!(
        api.handle(put(&version, Full::new(Bytes::from_static(text))))
            .await
            .status(),
        StatusCode::OK
    );
    let actual = app.configuration_status().version();
    let current = app.published_for_test().unwrap();
    send.send(Frame::data(Bytes::new())).await.unwrap();
    drop(send);
    assert_eq!(first.await.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(app.configuration_status().version(), actual);
    assert!(Arc::ptr_eq(&current, &app.published_for_test().unwrap()));
    assert_eq!(current.plan.upstreams.len(), 1);
    app.shutdown().await;
}

impl Body for PendingBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Pending
    }
}

#[tokio::test]
async fn slow_body_times_out_without_entering_configuration_pipeline() {
    let (app, api) = setup().await;
    tokio::time::pause();
    let version = app.configuration_status().version();
    assert_eq!(
        api.handle(put(&version.etag(), PendingBody)).await.status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert_eq!(api.requests.available_permits(), 8);
    assert_eq!(app.configuration_status().version(), version);
    tokio::time::resume();
    app.shutdown().await;
}
