//! SETTINGS observation is needed only while a checkout waits for capacity.
use std::future::{Future, pending};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};

pub(super) struct Demand {
    waiting: watch::Sender<usize>,
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            waiting: watch::channel(0).0,
        }
    }
}

impl Demand {
    pub fn wait(self: &Arc<Self>) -> Waiting {
        self.waiting.send_modify(|waiting| *waiting += 1);
        Waiting(self.clone())
    }

    pub fn observer(&self) -> Observer {
        Observer(self.waiting.subscribe())
    }
}

pub(super) struct Waiting(Arc<Demand>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.waiting.send_modify(|waiting| *waiting -= 1);
    }
}

pub(super) struct Observer(watch::Receiver<usize>);

impl Observer {
    // Hyper runs its H2 IO separately from the dispatch future. A SETTINGS-only
    // update may not wake dispatch, so waiting checkouts need a bounded sample
    // delay. With no demand only the connection or a demand change wakes us.
    pub async fn drive<F: Future>(mut self, connection: F, io_events: Arc<Notify>) -> F::Output {
        tokio::pin!(connection);
        loop {
            tokio::select! {
                result = &mut connection => return result,
                _ = io_events.notified() => {},
                _ = self.changed_or_tick() => {},
            }
        }
    }

    async fn changed_or_tick(&mut self) {
        let waiting = *self.0.borrow_and_update() != 0;
        let changed = if waiting {
            tokio::select! {
                changed = self.0.changed() => changed,
                _ = tokio::time::sleep(Duration::from_millis(50)) => return,
            }
        } else {
            self.0.changed().await
        };
        if changed.is_err() {
            // Pool ownership can end before the dispatch driver finishes.
            // Closing the watch must not turn the driver into a busy loop.
            pending::<()>().await;
        }
    }
}

#[cfg(test)]
#[path = "demand_tests.rs"]
mod tests;
