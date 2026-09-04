//! Counting a request the way an operator counts one.
//!
//! A handler returning means the response head was decided, not that anything
//! reached the client. Measuring there reports a ten-minute download as
//! instant, a cancelled one as a success, and a compressed body at its
//! uncompressed size. So the measurement is taken where the body ends — which,
//! for a body nobody read to the end, is where it is dropped.
//!
//! This is the same shape as the `access-log` directive, and deliberately not
//! the same code: logging is something a configuration asks for, and metrics
//! are not optional.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use gatel_observe::metrics::Route as RouteMetrics;
use salvo_core::http::{ResBody, Response};

/// One request in progress.
pub struct Measure {
    route: Arc<RouteMetrics>,
    started: Instant,
}

impl Measure {
    /// Begin. The in-flight gauge goes up here and down when the body ends,
    /// however it ends.
    pub fn start(route: Arc<RouteMetrics>) -> Self {
        route.in_flight.increment();
        Self {
            route,
            started: Instant::now(),
        }
    }

    fn finish(&self, status: u16, bytes: u64) {
        self.route.in_flight.decrement();
        self.route.completed(status, self.started.elapsed(), bytes);
    }
}

/// Take the measurement, wrapping the body if it has not finished yet.
///
/// Called after the whole chain has run, so what it counts is the body that
/// actually goes out — compressed, if something compressed it.
pub fn observe(response: &mut Response, measure: Measure) {
    let status = response.status_code.map(|code| code.as_u16()).unwrap_or(200);
    match std::mem::take(response.body_mut()) {
        ResBody::None => {
            measure.finish(status, 0);
            *response.body_mut() = ResBody::None;
        }
        ResBody::Once(bytes) => {
            measure.finish(status, bytes.len() as u64);
            *response.body_mut() = ResBody::Once(bytes);
        }
        ResBody::Chunks(chunks) => {
            measure.finish(status, chunks.iter().map(|chunk| chunk.len() as u64).sum());
            *response.body_mut() = ResBody::Chunks(chunks);
        }
        streaming => {
            *response.body_mut() = ResBody::Boxed(Box::pin(MeasuredBody {
                inner: streaming,
                measure: Some(measure),
                status,
                sent: 0,
            }));
        }
    }
}

pin_project_lite::pin_project! {
    struct MeasuredBody {
        #[pin]
        inner: ResBody,
        measure: Option<Measure>,
        status: u16,
        sent: u64,
    }

    impl PinnedDrop for MeasuredBody {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            // A body dropped before its end is a request the client abandoned.
            // Not recording it here would leave the in-flight gauge climbing
            // for the life of the process.
            if let Some(measure) = this.measure.take() {
                measure.finish(*this.status, *this.sent);
            }
        }
    }
}

impl http_body::Body for MeasuredBody {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    *this.sent += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(measure) = this.measure.take() {
                    measure.finish(*this.status, *this.sent);
                }
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => {
                if let Some(measure) = this.measure.take() {
                    measure.finish(*this.status, *this.sent);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
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
    use gatel_observe::Class;
    use http_body::Body as _;
    use salvo_core::http::StatusCode;

    use super::*;

    fn response(body: ResBody, status: u16) -> Response {
        let mut response = Response::new();
        response.status_code(StatusCode::from_u16(status).unwrap());
        response.body(body);
        response
    }

    /// A body that has not finished, so `observe` has to wrap it. `ResBody`
    /// cannot be boxed into itself, and a real streaming body is what this is
    /// standing in for anyway.
    struct Streaming(Option<Bytes>);

    impl http_body::Body for Streaming {
        type Data = Bytes;
        type Error = salvo_core::BoxedError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(self.0.take().map(|bytes| Ok(http_body::Frame::data(bytes))))
        }
    }

    fn streaming(payload: &'static [u8], status: u16) -> Response {
        response(
            ResBody::Boxed(Box::pin(Streaming(Some(Bytes::from_static(payload))))),
            status,
        )
    }

    #[test]
    fn a_body_already_in_memory_is_counted_at_once() {
        let route = Arc::new(RouteMetrics::default());
        let mut response = response(ResBody::Once(Bytes::from_static(b"hello")), 200);
        observe(&mut response, Measure::start(Arc::clone(&route)));
        assert_eq!(route.requests(Class::Success), 1);
        assert_eq!(route.response_bytes.get(), 5);
        assert_eq!(route.in_flight.get(), 0);
    }

    #[test]
    fn an_empty_body_still_counts_as_a_request() {
        let route = Arc::new(RouteMetrics::default());
        let mut response = response(ResBody::None, 404);
        observe(&mut response, Measure::start(Arc::clone(&route)));
        assert_eq!(route.requests(Class::ClientError), 1);
        assert_eq!(route.response_bytes.get(), 0);
    }

    #[tokio::test]
    async fn a_streaming_body_is_counted_when_it_ends() {
        let route = Arc::new(RouteMetrics::default());
        let mut response = streaming(b"abc", 200);
        observe(&mut response, Measure::start(Arc::clone(&route)));
        assert_eq!(route.in_flight.get(), 1, "not finished yet");
        assert_eq!(route.requests(Class::Success), 0);

        let mut body = std::mem::take(response.body_mut());
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            frame.unwrap();
        }
        assert_eq!(route.requests(Class::Success), 1);
        assert_eq!(route.response_bytes.get(), 3);
        assert_eq!(route.in_flight.get(), 0);
    }

    #[test]
    fn a_body_the_client_abandoned_does_not_leak_the_gauge() {
        let route = Arc::new(RouteMetrics::default());
        let mut response = streaming(b"abc", 200);
        observe(&mut response, Measure::start(Arc::clone(&route)));
        assert_eq!(route.in_flight.get(), 1);

        drop(response);
        assert_eq!(route.in_flight.get(), 0, "a dropped body is a finished request");
        assert_eq!(route.requests(Class::Success), 1);
        assert_eq!(route.response_bytes.get(), 0, "nothing reached the client");
    }
}
