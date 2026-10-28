//! Bounded cache fills. Giving up on caching must leave the original stream intact.
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use salvo_core::http::ResBody;

const MAX_FILLS: usize = 64;

/// Separate from retained entries: unfinished fills reserve at most max-size
/// bytes in total, with a task cap even for zero-length responses. This is a
/// logical payload bound, not a bound on transport buffers or allocator RSS.
#[derive(Debug)]
pub(super) struct Budget {
    max_bytes: u64,
    used: Mutex<(u64, usize)>,
}

impl Budget {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            used: Mutex::new((0, 0)),
        }
    }

    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        if used.1 >= MAX_FILLS || bytes > self.max_bytes.saturating_sub(used.0) {
            return None;
        }
        used.0 += bytes;
        used.1 += 1;
        Some(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

pub(super) struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|p| p.into_inner());
        used.0 -= self.bytes;
        used.1 -= 1;
    }
}

pub(super) enum Collected {
    Whole(Bytes, Option<Reservation>),
    TooLarge(ResBody),
    /// A trailer or body error cannot be represented in a cached bytes entry.
    Uncacheable(ResBody),
}

/// Check before polling, then stop at the first frame that crosses the bound.
/// On fallback the already-read prefix, current frame and remaining body are
/// replayed in order, including trailers and the original error.
pub(super) async fn collect(mut body: ResBody, limit: u64, reservation: Option<Reservation>) -> Collected {
    if body.size_hint().lower() > limit {
        return Collected::TooLarge(body);
    }
    if let ResBody::Once(bytes) = &body {
        return Collected::Whole(bytes.clone(), reservation);
    }
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                return Collected::Uncacheable(replay(
                    bytes.freeze(),
                    Err(Box::new(error)),
                    body,
                    reservation,
                ));
            }
        };
        let data = match frame.into_data() {
            Ok(data) => data,
            Err(frame) => {
                return Collected::Uncacheable(replay(bytes.freeze(), Ok(frame), body, reservation));
            }
        };
        if (data.len() as u64) > limit.saturating_sub(bytes.len() as u64) {
            return Collected::TooLarge(replay(bytes.freeze(), Ok(Frame::data(data)), body, reservation));
        }
        bytes.extend_from_slice(&data);
    }
    Collected::Whole(bytes.freeze(), reservation)
}

fn replay(
    bytes: Bytes,
    frame: Result<Frame<Bytes>, salvo_core::BoxedError>,
    body: ResBody,
    reservation: Option<Reservation>,
) -> ResBody {
    let mut prefix = VecDeque::new();
    if !bytes.is_empty() {
        prefix.push_back(Ok(Frame::data(bytes)));
    }
    prefix.push_back(frame);
    ResBody::Boxed(Box::pin(Prefix {
        prefix,
        body,
        reservation,
    }))
}

struct Prefix {
    prefix: VecDeque<Result<Frame<Bytes>, salvo_core::BoxedError>>,
    body: ResBody,
    reservation: Option<Reservation>,
}

impl Body for Prefix {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Some(frame) = self.prefix.pop_front() {
            Poll::Ready(Some(frame))
        } else {
            self.reservation.take();
            Pin::new(&mut self.body)
                .poll_frame(cx)
                .map(|frame| frame.map(|result| result.map_err(|error| Box::new(error) as Self::Error)))
        }
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_empty() && self.body.is_end_stream()
    }
}

#[cfg(test)]
#[path = "fill_tests.rs"]
mod tests;
