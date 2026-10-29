//! One-off diagnostic only. Never included in the proposed product diff.
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use salvo_core::http::ResBody;

static NEXT: AtomicU64 = AtomicU64::new(1);

pub(crate) fn wrap(inner: ResBody, expected: u64) -> ResBody {
    ResBody::Boxed(Box::pin(Timing {
        inner: Box::pin(inner),
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        expected,
        first: None,
        last_ready: None,
        pending: None,
        producer: Duration::ZERO,
        consumer: Duration::ZERO,
        producer_max: Duration::ZERO,
        consumer_max: Duration::ZERO,
        poll_cpu: Duration::ZERO,
        bytes: 0,
        frames: 0,
        finished: false,
    }))
}

struct Timing {
    inner: Pin<Box<ResBody>>,
    id: u64,
    expected: u64,
    first: Option<Instant>,
    last_ready: Option<Instant>,
    pending: Option<Instant>,
    producer: Duration,
    consumer: Duration,
    producer_max: Duration,
    consumer_max: Duration,
    poll_cpu: Duration,
    bytes: u64,
    frames: u64,
    finished: bool,
}

impl Timing {
    fn account_pending(&mut self, now: Instant) {
        if let Some(started) = self.pending.take() {
            let elapsed = now.duration_since(started);
            self.producer += elapsed;
            self.producer_max = self.producer_max.max(elapsed);
        }
    }

    fn finish(&mut self, now: Instant, outcome: &str) {
        if self.finished || self.first.is_none() {
            return;
        }
        self.account_pending(now);
        if let Some(previous) = self.last_ready.take() {
            let elapsed = now.duration_since(previous);
            self.consumer += elapsed;
            self.consumer_max = self.consumer_max.max(elapsed);
        }
        self.finished = true;
        eprintln!(
            "BODY_TIMING {{\"id\":{},\"outcome\":\"{}\",\"bytes\":{},\"expected\":{},\"frames\":{},\"wall_us\":{},\"producer_wait_us\":{},\"consumer_gap_us\":{},\"producer_max_us\":{},\"consumer_max_us\":{},\"poll_cpu_us\":{}}}",
            self.id,
            outcome,
            self.bytes,
            self.expected,
            self.frames,
            now.duration_since(self.first.unwrap()).as_micros(),
            self.producer.as_micros(),
            self.consumer.as_micros(),
            self.producer_max.as_micros(),
            self.consumer_max.as_micros(),
            self.poll_cpu.as_micros(),
        );
    }
}

impl http_body::Body for Timing {
    type Data = <ResBody as http_body::Body>::Data;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let me = self.get_mut();
        let before = Instant::now();
        me.first.get_or_insert(before);
        if me.pending.is_none() {
            if let Some(previous) = me.last_ready.take() {
                let elapsed = before.duration_since(previous);
                me.consumer += elapsed;
                me.consumer_max = me.consumer_max.max(elapsed);
            }
            me.pending = Some(before);
        }
        let result = me.inner.as_mut().poll_frame(cx);
        let after = Instant::now();
        me.poll_cpu += after.duration_since(before);
        match &result {
            Poll::Pending => {}
            Poll::Ready(Some(Ok(frame))) => {
                me.account_pending(after);
                me.frames += 1;
                if let Some(bytes) = frame.data_ref() {
                    me.bytes += bytes.len() as u64;
                }
                me.last_ready = Some(after);
            }
            Poll::Ready(Some(Err(_))) => me.finish(after, "error"),
            Poll::Ready(None) => me.finish(after, "eof"),
        }
        result.map(|frame| frame.map(|result| result.map_err(|error| Box::new(error) as salvo_core::BoxedError)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        self.finish(Instant::now(), if self.bytes == self.expected { "emitted" } else { "dropped" });
    }
}
