use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;

#[derive(Default)]
pub struct BackendActivityTracker {
    counts: DashMap<String, Arc<AtomicUsize>>,
}

impl BackendActivityTracker {
    pub fn acquire(&self, key: impl Into<String>) -> BackendActivityGuard {
        let key = key.into();
        let counter = self
            .counts
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, Ordering::Relaxed);
        BackendActivityGuard {
            counter: Some(counter),
        }
    }

    pub fn active(&self, key: &str) -> usize {
        self.counts
            .get(key)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

pub struct BackendActivityGuard {
    counter: Option<Arc<AtomicUsize>>,
}

impl Drop for BackendActivityGuard {
    fn drop(&mut self) {
        if let Some(counter) = self.counter.take() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_active_backend_requests() {
        let tracker = BackendActivityTracker::default();
        assert_eq!(tracker.active("api/route/group/target"), 0);

        let guard_a = tracker.acquire("api/route/group/target");
        let guard_b = tracker.acquire("api/route/group/target");
        assert_eq!(tracker.active("api/route/group/target"), 2);

        drop(guard_a);
        assert_eq!(tracker.active("api/route/group/target"), 1);

        drop(guard_b);
        assert_eq!(tracker.active("api/route/group/target"), 0);
    }
}
