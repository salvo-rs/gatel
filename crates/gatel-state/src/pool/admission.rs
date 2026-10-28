//! H1 permits retain a target capacity while occupied slots drain after a shrink.
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

pub(super) struct Admission {
    permits: Arc<Semaphore>,
    state: Mutex<Policy>,
}

pub(super) struct Policy {
    pub config: gatel_plan::Pool,
    pub allocated: u32,
}

impl Admission {
    pub fn new(config: gatel_plan::Pool, closed: bool) -> Arc<Self> {
        let permits = Arc::new(Semaphore::new(config.max_per_endpoint as usize));
        if closed {
            permits.close();
        }
        Arc::new(Self {
            permits,
            state: Mutex::new(Policy {
                config,
                allocated: config.max_per_endpoint,
            }),
        })
    }

    pub fn policy(&self) -> MutexGuard<'_, Policy> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn adopt(&self, config: gatel_plan::Pool) -> MutexGuard<'_, Policy> {
        let mut state = self.policy();
        state.config = config;
        if config.max_per_endpoint > state.allocated {
            self.permits
                .add_permits((config.max_per_endpoint - state.allocated) as usize);
            state.allocated = config.max_per_endpoint;
        } else {
            let removed = self
                .permits
                .forget_permits((state.allocated - config.max_per_endpoint) as usize);
            state.allocated -= removed as u32;
        }
        state
    }

    pub fn available(&self) -> u32 {
        self.permits.available_permits() as u32
    }
    #[cfg(test)]
    pub fn in_use(&self) -> u32 {
        self.policy().allocated.saturating_sub(self.available())
    }
    pub fn close(&self) {
        self.permits.close();
    }
    pub fn is_closed(&self) -> bool {
        self.permits.is_closed()
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<Permit, AcquireError> {
        loop {
            let permit = self.permits.clone().acquire_owned().await?;
            let mut state = self.policy();
            if state.allocated > state.config.max_per_endpoint {
                // An assigned-but-unpolled waiter can be cancelled without
                // constructing our guard. Consume that returned shrink debt
                // before approving another request.
                permit.forget();
                state.allocated -= 1;
                continue;
            }
            return Ok(Permit {
                owner: self.clone(),
                permit: Some(permit),
            });
        }
    }
}

pub(super) struct Permit {
    owner: Arc<Admission>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.owner.policy();
        let permit = self.permit.take().expect("a live reservation owns its permit");
        if state.allocated > state.config.max_per_endpoint {
            permit.forget();
            state.allocated -= 1;
        } else {
            // Return and resize share the lock: neither can miss the other.
            drop(permit);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    #[tokio::test]
    async fn an_assigned_cancelled_waiter_cannot_escape_a_shrink() {
        let config = gatel_plan::Pool {
            max_per_endpoint: 2,
            ..Default::default()
        };
        let admission = Admission::new(config, false);
        let first = admission.acquire().await.unwrap();
        let second = admission.acquire().await.unwrap();
        let mut waiting = Box::pin(admission.acquire());
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
        drop(first); // The semaphore assigns this to the unpolled waiter.
        drop(admission.adopt(gatel_plan::Pool {
            max_per_endpoint: 1,
            ..config
        }));
        drop(waiting);
        let mut blocked = Box::pin(admission.acquire());
        assert!(matches!(blocked.as_mut().poll(&mut context), Poll::Pending));
        drop(second);
        assert!(blocked.await.is_ok());
    }

    #[tokio::test]
    async fn repeated_resize_and_cancel_restore_exact_capacity() {
        let config = gatel_plan::Pool {
            max_per_endpoint: 3,
            ..Default::default()
        };
        let admission = Admission::new(config, false);
        let a = admission.acquire().await.unwrap();
        let b = admission.acquire().await.unwrap();
        let c = admission.acquire().await.unwrap();
        drop(admission.adopt(gatel_plan::Pool {
            max_per_endpoint: 1,
            ..config
        }));
        drop(a);
        drop(admission.adopt(config));
        drop((b, c));
        assert_eq!(admission.available(), 3);
        assert_eq!(admission.in_use(), 0);
        admission.close();
        assert!(admission.acquire().await.is_err());
    }
}
