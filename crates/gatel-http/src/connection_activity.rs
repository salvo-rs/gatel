//! HTTP idle means no complete request is being handled or sent. Transport
//! chatter and partial heads do not extend it; active response streams do.
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct Activity(Arc<watch::Sender<State>>);

#[derive(Clone, Copy)]
struct State {
    active: usize,
    idle_since: Instant,
}

impl Activity {
    pub fn new() -> Self {
        Self(Arc::new(
            watch::channel(State {
                active: 0,
                idle_since: Instant::now(),
            })
            .0,
        ))
    }

    pub fn begin_http(&self) {
        self.0.send_modify(|state| state.idle_since = Instant::now());
    }

    pub fn request(&self) -> Request {
        self.0.send_modify(|state| state.active += 1);
        Request(self.clone())
    }

    pub async fn expired(&self, timeout: Duration) {
        let mut changed = self.0.subscribe();
        loop {
            let state = *changed.borrow_and_update();
            if state.active == 0 {
                tokio::select! {
                    biased;
                    result = changed.changed() => { if result.is_err() { return; } },
                    _ = tokio::time::sleep_until(state.idle_since + timeout) => return,
                }
            } else if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

pub(crate) struct Request(Activity);

impl Drop for Request {
    fn drop(&mut self) {
        self.0.0.send_modify(|state| {
            state.active -= 1;
            if state.active == 0 {
                state.idle_since = Instant::now();
            }
        });
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct Body<B> {
        #[pin]
        inner: B,
        request: Option<Request>,
    }
}

impl<B: http_body::Body> Body<B> {
    pub fn new(inner: B, request: Request) -> Self {
        let request = (!inner.is_end_stream()).then_some(request);
        Self { inner, request }
    }
}

impl<B: http_body::Body> http_body::Body for Body<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        let result = this.inner.as_mut().poll_frame(cx);
        if matches!(&result, Poll::Ready(None | Some(Err(_)))) || this.inner.is_end_stream() {
            this.request.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn only_the_last_finished_request_starts_the_idle_period() {
        let activity = Activity::new();
        activity.begin_http();
        let a = activity.request();
        let b = activity.request();
        let expired = activity.expired(Duration::from_secs(1));
        tokio::pin!(expired);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut expired)
                .await
                .is_err()
        );
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut expired)
                .await
                .is_err()
        );
        drop(b);
        assert!(
            tokio::time::timeout(Duration::from_millis(900), &mut expired)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_millis(200), expired)
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_an_unfinished_response_releases_request_activity() {
        let activity = Activity::new();
        let pending = http_body_util::StreamBody::new(futures_util::stream::pending::<
            Result<http_body::Frame<bytes::Bytes>, std::convert::Infallible>,
        >());
        let body = Body::new(pending, activity.request());
        let expired = activity.expired(Duration::from_secs(1));
        tokio::pin!(expired);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut expired)
                .await
                .is_err()
        );
        drop(body);
        tokio::time::timeout(Duration::from_secs(2), expired)
            .await
            .unwrap();
    }
}
