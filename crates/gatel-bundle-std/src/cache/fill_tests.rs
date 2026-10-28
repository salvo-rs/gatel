use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

use http_body_util::StreamBody;

use super::*;

fn stream(frames: Vec<Result<Frame<Bytes>, salvo_core::BoxedError>>) -> ResBody {
    ResBody::Boxed(Box::pin(StreamBody::new(futures_util::stream::iter(frames))))
}

#[tokio::test]
async fn unknown_length_stops_at_limit_and_replays_the_prefix_and_remaining_frames() {
    let polls = Arc::new(AtomicUsize::new(0));
    let polled = polls.clone();
    let frames = [b"ab".as_slice(), b"cdef", b"gh"]
        .into_iter()
        .map(|bytes| Ok::<_, salvo_core::BoxedError>(Frame::data(Bytes::copy_from_slice(bytes))));
    let body = ResBody::Boxed(Box::pin(StreamBody::new(futures_util::StreamExt::inspect(
        futures_util::stream::iter(frames),
        move |_| {
            polled.fetch_add(1, Ordering::Relaxed);
        },
    ))));
    let budget = Arc::new(Budget::new(4));
    let Collected::TooLarge(body) = collect(body, 4, budget.reserve(4)).await else {
        panic!("must fall back")
    };
    assert_eq!(
        polls.load(Ordering::Relaxed),
        2,
        "must not consume the remaining stream"
    );
    assert!(
        budget.reserve(1).is_none(),
        "prefix retains its reservation until forwarded"
    );
    assert_eq!(body.collect().await.unwrap().to_bytes(), "abcdefgh");
    assert!(budget.reserve(4).is_some());
}

#[tokio::test]
async fn trailers_and_errors_are_preserved_instead_of_silently_converting_to_bytes() {
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-checksum", "done".parse().unwrap());
    let body = stream(vec![
        Ok(Frame::data(Bytes::from_static(b"prefix"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let Collected::Uncacheable(body) = collect(body, 1024, None).await else {
        panic!("trailers cannot be cached")
    };
    let collected = body.collect().await.unwrap();
    assert_eq!(collected.trailers().unwrap()["x-checksum"], "done");
    assert_eq!(collected.to_bytes(), "prefix");

    let body = stream(vec![
        Ok(Frame::data(Bytes::from_static(b"partial"))),
        Err(Box::new(io::Error::other("injected read failure"))),
    ]);
    let Collected::Uncacheable(mut body) = collect(body, 1024, None).await else {
        panic!("errors cannot be cached")
    };
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "partial"
    );
    assert!(
        body.frame()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("injected read failure")
    );
}

#[test]
fn reservations_bound_bytes_and_zero_length_tasks_and_release_on_drop() {
    let budget = Arc::new(Budget::new(8));
    let first = budget.reserve(5).unwrap();
    assert!(budget.reserve(4).is_none());
    let second = budget.reserve(3).unwrap();
    assert!(budget.reserve(1).is_none());
    drop(first);
    assert!(budget.reserve(5).is_some());
    drop(second);
    let reservations: Vec<_> = (0..MAX_FILLS).map(|_| budget.reserve(0).unwrap()).collect();
    assert!(budget.reserve(0).is_none());
    drop(reservations);
    assert!(budget.reserve(8).is_some());
}

#[tokio::test]
async fn cancelling_an_incomplete_fill_releases_its_reservation_and_body() {
    let budget = Arc::new(Budget::new(8));
    let permit = budget.reserve(8).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let body = ResBody::Boxed(Box::pin(StreamBody::new(futures_util::stream::poll_fn(
        move |_| {
            if let Some(started) = started.take() {
                started.send(()).unwrap();
            }
            Poll::<Option<Result<Frame<Bytes>, salvo_core::BoxedError>>>::Pending
        },
    ))));
    let task = tokio::spawn(collect(body, 8, Some(permit)));
    ready.await.unwrap();
    assert!(budget.reserve(1).is_none());
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert!(budget.reserve(8).is_some());
}

#[tokio::test]
async fn dropping_an_unpolled_fallback_releases_its_prefix_reservation() {
    let budget = Arc::new(Budget::new(4));
    let body = stream(vec![
        Ok(Frame::data(Bytes::from_static(b"ab"))),
        Ok(Frame::data(Bytes::from_static(b"cdef"))),
    ]);
    let Collected::TooLarge(body) = collect(body, 4, budget.reserve(4)).await else {
        panic!("must fall back")
    };
    assert!(budget.reserve(1).is_none());
    drop(body);
    assert!(budget.reserve(4).is_some());
}
