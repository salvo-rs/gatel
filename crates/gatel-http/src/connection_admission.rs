//! Shared connection admission with deferred shrinking of occupied capacity.
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) struct Admission {
    slots: Arc<Semaphore>,
    capacity: Mutex<Capacity>,
}

struct Capacity {
    allocated: u32,
    wanted: u32,
}

impl Admission {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(0)),
            capacity: Mutex::new(Capacity {
                allocated: 0,
                wanted: 0,
            }),
        })
    }

    pub fn resize(&self, wanted: u32) {
        let mut capacity = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        capacity.wanted = wanted;
        if wanted > capacity.allocated {
            self.slots.add_permits((wanted - capacity.allocated) as usize);
            capacity.allocated = wanted;
        } else {
            let removed = self.slots.forget_permits((capacity.allocated - wanted) as usize);
            capacity.allocated -= removed as u32;
        }
    }

    pub async fn acquire(self: &Arc<Self>) -> Permit {
        // The semaphore is private and never closed. Its FIFO wait queue also
        // shares released capacity across listeners with pending connections.
        loop {
            let slot = self
                .slots
                .clone()
                .acquire_owned()
                .await
                .expect("admission remains open");
            let mut capacity = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
            if capacity.allocated > capacity.wanted {
                // A semaphore waiter can reserve a slot before polling Ready.
                // Cancelling it returns that slot without creating our guard;
                // consume any outstanding shrink debt before admitting it.
                slot.forget();
                capacity.allocated -= 1;
                continue;
            }
            return Permit {
                owner: self.clone(),
                slot: Some(slot),
            };
        }
    }
}

pub(crate) struct Permit {
    owner: Arc<Admission>,
    slot: Option<OwnedSemaphorePermit>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut capacity = self.owner.capacity.lock().unwrap_or_else(|p| p.into_inner());
        let slot = self.slot.take().expect("a live permit owns its slot");
        if capacity.allocated > capacity.wanted {
            // A busy connection kept its permit across a shrink. Consume the
            // debt before the semaphore can assign it to a queued connection.
            slot.forget();
            capacity.allocated -= 1;
        } else {
            // Return under the same lock as resize: a shrink must not miss a
            // permit that was approved for return just before its decision.
            drop(slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shrinking_occupied_slots_does_not_admit_waiters_on_the_old_capacity() {
        let admission = Admission::new();
        admission.resize(3);
        let first = admission.acquire().await;
        let second = admission.acquire().await;
        let third = admission.acquire().await;
        admission.resize(1);
        drop(first);
        drop(second);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), admission.acquire())
                .await
                .is_err()
        );
        drop(third);
        let _only = admission.acquire().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), admission.acquire())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn raising_capacity_cancels_unnecessary_shrink_debt() {
        let admission = Admission::new();
        admission.resize(3);
        let a = admission.acquire().await;
        let b = admission.acquire().await;
        let c = admission.acquire().await;
        admission.resize(1);
        admission.resize(3);
        drop((a, b, c));
        let _a = admission.acquire().await;
        let _b = admission.acquire().await;
        let _c = admission.acquire().await;
    }

    #[tokio::test]
    async fn cancelling_a_waiter_with_an_assigned_slot_cannot_escape_a_shrink() {
        let admission = Admission::new();
        admission.resize(2);
        let a = admission.acquire().await;
        let b = admission.acquire().await;
        let waiting = admission.clone();
        let waiter = tokio::spawn(async move { waiting.acquire().await });
        tokio::task::yield_now().await;
        drop(a); // Reserve this slot for the waiter, but do not poll it yet.
        admission.resize(1);
        waiter.abort();
        let _ = waiter.await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), admission.acquire())
                .await
                .is_err()
        );
        drop(b);
        let _only = admission.acquire().await;
    }
}
