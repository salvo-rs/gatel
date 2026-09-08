//! Shared cache-target ordering without holding configuration locks over I/O.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
pub(crate) struct Orders {
    gates: Mutex<BTreeMap<PathBuf, Weak<AsyncMutex<()>>>>,
}

impl Orders {
    /// Key must use a canonical parent directory so lexical aliases converge.
    pub async fn acquire(&self, path: PathBuf) -> OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self.gates.lock().unwrap_or_else(|p| p.into_inner());
            gates.retain(|_, gate| gate.strong_count() != 0);
            match gates.get(&path).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(AsyncMutex::new(()));
                    gates.insert(path, Arc::downgrade(&gate));
                    gate
                }
            }
        };
        gate.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn same_target_serializes_other_targets_progress_and_cancellation_releases() {
        let orders = Arc::new(Orders::default());
        let (ready, received) = tokio::sync::oneshot::channel();
        let worker_orders = orders.clone();
        let task = tokio::spawn(async move {
            let _guard = worker_orders.acquire("one".into()).await;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        received.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), orders.acquire("one".into()))
                .await
                .is_err()
        );
        let other = tokio::time::timeout(Duration::from_secs(1), orders.acquire("two".into()))
            .await
            .unwrap();
        task.abort();
        let _ = task.await;
        let first = tokio::time::timeout(Duration::from_secs(1), orders.acquire("one".into()))
            .await
            .unwrap();
        drop(other);
        drop(first);
        let _last = orders.acquire("last".into()).await;
        assert_eq!(
            orders.gates.lock().unwrap().len(),
            1,
            "inactive cache targets do not accumulate"
        );
    }
}
