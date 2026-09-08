//! Admission and drain for synchronous host configuration transactions.
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Default)]
struct State {
    closed: bool,
    running: usize,
}

#[derive(Default)]
pub(super) struct Submissions {
    state: Mutex<State>,
    changed: Notify,
}

pub(super) struct Admission(Arc<Submissions>);

impl Submissions {
    pub fn enter(self: &Arc<Self>) -> Option<Admission> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return None;
        }
        state.running = state.running.checked_add(1).expect("submission count exhausted");
        Some(Admission(self.clone()))
    }

    pub fn close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
    }

    /// Cancellation leaves ownership/counts intact; any later caller can retry.
    /// Register before checking the count so the final drop cannot be missed.
    pub async fn drained(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().unwrap_or_else(|p| p.into_inner()).running == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state.running -= 1;
        let empty = state.running == 0;
        drop(state);
        if empty {
            self.0.changed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Waker};

    use super::*;

    #[tokio::test]
    async fn cancelled_and_concurrent_drains_retain_admission_ownership() {
        let submissions = Arc::new(Submissions::default());
        let first = submissions.enter().unwrap();
        let second = submissions.enter().unwrap();
        submissions.close();
        assert!(submissions.enter().is_none());
        let mut cx = Context::from_waker(Waker::noop());
        let mut cancelled = Box::pin(submissions.drained());
        assert!(cancelled.as_mut().poll(&mut cx).is_pending());
        drop(cancelled);
        let mut a = Box::pin(submissions.drained());
        let mut b = Box::pin(submissions.drained());
        assert!(a.as_mut().poll(&mut cx).is_pending());
        assert!(b.as_mut().poll(&mut cx).is_pending());
        drop(first);
        assert!(a.as_mut().poll(&mut cx).is_pending());
        drop(second);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(a, b);
        })
        .await
        .unwrap();
        submissions.close();
        assert!(submissions.enter().is_none());
        submissions.drained().await;
    }

    #[tokio::test]
    async fn final_release_wakes_a_registered_waiter() {
        let submissions = std::sync::Arc::new(Submissions::default());
        let admission = submissions.enter().unwrap();
        submissions.close();
        let (ready, registered) = tokio::sync::oneshot::channel();
        let waiter = {
            let submissions = submissions.clone();
            tokio::spawn(async move {
                let drained = submissions.drained();
                tokio::pin!(drained);
                let mut ready = Some(ready);
                std::future::poll_fn(|cx| {
                    let result = drained.as_mut().poll(cx);
                    if result.is_pending()
                        && let Some(ready) = ready.take()
                    {
                        let _ = ready.send(());
                    }
                    result
                })
                .await;
            })
        };
        registered.await.unwrap();
        drop(admission);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn unwinding_releases_admission() {
        let submissions = Arc::new(Submissions::default());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _admission = submissions.enter().unwrap();
            panic!("fixture");
        }));
        assert!(result.is_err());
        submissions.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), submissions.drained())
            .await
            .unwrap();
    }
}
