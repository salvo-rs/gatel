//! Refusing a request body that is too large.
//!
//! Two cases, because a client can tell the truth, lie, or say nothing. A
//! declared length over the limit is refused before anything reads a byte. A
//! body with no declared length — chunked, or HTTP/2 — is counted as it goes and
//! cut off the moment it passes the limit, which is the only point at which
//! there is anything to refuse.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use salvo_core::http::{ReqBody, Request};

/// What a declared length says about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declared {
    /// Nothing was declared, or what was declared fits.
    Acceptable,
    /// The client said, up front, that it is sending more than the limit.
    TooLarge(u64),
}

/// Bound the body of this request.
///
/// Returns what the declared length said, so a caller can answer 413 before
/// reading anything rather than after reading too much.
pub fn limit(request: &mut Request, max: u64) -> Declared {
    if let Some(declared) = content_length(request)
        && declared > max
    {
        return Declared::TooLarge(declared);
    }
    let body = std::mem::take(request.body_mut());
    *request.body_mut() = ReqBody::Boxed {
        inner: Box::pin(Limited {
            inner: body,
            max,
            seen: 0,
            done: false,
        }),
        fuse_config: None,
    };
    Declared::Acceptable
}

fn content_length(request: &Request) -> Option<u64> {
    request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

pin_project_lite::pin_project! {
    struct Limited {
        #[pin]
        inner: ReqBody,
        max: u64,
        seen: u64,
        done: bool,
    }
}

impl http_body::Body for Limited {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if data.len() as u64 > this.max.saturating_sub(*this.seen) {
                        *this.done = true;
                        this.inner.set(ReqBody::default());
                        // The frame is not passed on. Whatever is reading this
                        // body — a proxy writing to a backend, a form parser —
                        // sees the error rather than the excess.
                        return Poll::Ready(Some(Err(Box::new(gatel_plan::BodyTooLarge {
                            limit: *this.max,
                        }))));
                    }
                    *this.seen += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                *this.done = true;
                this.inner.set(ReqBody::default());
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => {
                *this.done = true;
                this.inner.set(ReqBody::default());
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        if self.done {
            http_body::SizeHint::with_exact(0)
        } else {
            self.inner.size_hint()
        }
    }
}

#[cfg(test)]
mod tests {
    use http::header::HeaderValue;
    use http_body::Body as _;

    use super::*;

    struct Probe {
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail: bool,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl http_body::Body for Probe {
        type Data = Bytes;
        type Error = salvo_core::BoxedError;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            if self.fail {
                Poll::Ready(Some(Err(std::io::Error::other("broken body").into())))
            } else {
                Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(b"ab")))))
            }
        }
    }

    #[tokio::test]
    async fn failure_drops_source_and_fuses_even_if_consumer_polls_again() {
        for fail in [false, true] {
            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut body = Limited {
                inner: ReqBody::Boxed {
                    inner: Box::pin(Probe {
                        dropped: dropped.clone(),
                        fail,
                    }),
                    fuse_config: None,
                },
                max: 1,
                seen: 0,
                done: false,
            };
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .unwrap();
            assert!(frame.is_err());
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
            assert!(body.is_end_stream());
            assert_eq!(body.size_hint().exact(), Some(0));
            for _ in 0..2 {
                assert!(
                    std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                        .await
                        .is_none()
                );
            }
        }
    }

    #[tokio::test]
    async fn byte_counter_cannot_wrap_at_u64_boundary() {
        let mut body = Limited {
            inner: ReqBody::Once(Bytes::from_static(b"ab")),
            max: u64::MAX,
            seen: u64::MAX - 1,
            done: false,
        };
        let error = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            gatel_plan::BodyTooLarge::find(error.as_ref()),
            Some(gatel_plan::BodyTooLarge { limit: u64::MAX })
        );
        assert!(body.is_end_stream());
    }

    fn request(body: &'static [u8], declared: Option<&str>) -> Request {
        let mut request = Request::default();
        if let Some(declared) = declared {
            request.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                HeaderValue::from_str(declared).unwrap(),
            );
        }
        *request.body_mut() = ReqBody::Once(Bytes::from_static(body));
        request
    }

    async fn drain(request: &mut Request) -> Result<u64, String> {
        let mut body = std::mem::take(request.body_mut());
        let mut total = 0;
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let frame = frame.map_err(|error| error.to_string())?;
            if let Some(data) = frame.data_ref() {
                total += data.len() as u64;
            }
        }
        Ok(total)
    }

    #[test]
    fn a_declared_length_over_the_limit_is_refused_before_anything_is_read() {
        let mut request = request(b"", Some("9999"));
        assert_eq!(limit(&mut request, 1024), Declared::TooLarge(9999));
    }

    #[test]
    fn a_declared_length_that_fits_is_accepted() {
        let mut request = request(b"hello", Some("5"));
        assert_eq!(limit(&mut request, 1024), Declared::Acceptable);
    }

    #[test]
    fn a_nonsense_length_is_not_a_refusal() {
        // The header is rubbish, so it says nothing about the size. Whatever
        // arrives is counted instead.
        let mut request = request(b"hello", Some("not-a-number"));
        assert_eq!(limit(&mut request, 1024), Declared::Acceptable);
    }

    #[tokio::test]
    async fn a_body_within_the_limit_passes_through_unchanged() {
        let mut request = request(b"hello", None);
        limit(&mut request, 1024);
        assert_eq!(drain(&mut request).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn an_undeclared_body_is_cut_off_when_it_passes_the_limit() {
        // Nothing was declared, so the only place to refuse it is here.
        let mut request = request(b"0123456789", None);
        limit(&mut request, 4);
        let error = drain(&mut request).await.unwrap_err();
        assert!(error.contains("larger than the 4 byte limit"), "{error}");
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_limit_is_allowed() {
        let mut request = request(b"0123", None);
        limit(&mut request, 4);
        assert_eq!(drain(&mut request).await.unwrap(), 4);
    }
}
