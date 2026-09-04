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
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    *this.seen += data.len() as u64;
                    if *this.seen > *this.max {
                        // The frame is not passed on. Whatever is reading this
                        // body — a proxy writing to a backend, a form parser —
                        // sees the error rather than the excess.
                        return Poll::Ready(Some(Err(format!(
                            "request body is larger than the {} byte limit",
                            *this.max
                        )
                        .into())));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => Poll::Ready(None),
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
    use http::header::HeaderValue;
    use http_body::Body as _;

    use super::*;

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
