//! Cancellation-owned recovery for long-lived background services.

use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use gatel_observe::Telemetry;
use gatel_observe::workers::WorkerKind;
use tokio::sync::watch;

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// Run one child at a time, restarting unexpected exits without spawning a
/// detached child task. Unwinding panics are contained at the polling boundary;
/// panic=abort and process termination remain outside this contract.
pub(crate) async fn supervise<F, R>(
    service: WorkerKind,
    upstream: String,
    telemetry: Arc<Telemetry>,
    mut stop: watch::Receiver<bool>,
    mut start: F,
) where
    F: FnMut(watch::Receiver<bool>) -> R,
    R: Future<Output = ()>,
{
    let mut delay = INITIAL_DELAY;
    let metrics = telemetry.workers.get(service);
    loop {
        if *stop.borrow() || stop.has_changed().is_err() {
            return;
        }
        let started = tokio::time::Instant::now();
        let receiver = stop.clone();
        let panicked = {
            // Construct inside the guarded future so a factory panic is also
            // contained. Dropping this scope drops all child-owned operations.
            let mut child = Box::pin(async {
                metrics.starts.increment();
                start(receiver).await
            });
            tokio::select! {
                biased;
                _ = stop.changed() => return,
                panicked = poll_fn(|cx| match catch_unwind(AssertUnwindSafe(|| child.as_mut().poll(cx))) {
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(())) => Poll::Ready(false),
                    Err(_) => Poll::Ready(true),
                }) => panicked,
            }
        };
        if *stop.borrow() || stop.has_changed().is_err() {
            return;
        }
        if started.elapsed() >= STABLE_AFTER {
            delay = INITIAL_DELAY;
        }
        if panicked {
            metrics.panicked.increment();
        } else {
            metrics.returned.increment();
        }
        let backoff = metrics.enter_backoff();
        tracing::warn!(
            target: "gatel::runtime",
            event = "worker_restart_scheduled",
            service = service.as_str(),
            upstream,
            panicked,
            delay_ms = delay.as_millis() as u64,
            "background worker exited unexpectedly",
        );
        tokio::select! {
            biased;
            _ = stop.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(MAX_DELAY);
        drop(backoff);
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
