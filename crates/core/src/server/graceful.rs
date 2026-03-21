//! Graceful shutdown coordinator.
//!
//! Tracks active connections and provides a mechanism to drain them on
//! shutdown.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Coordinates graceful shutdown across the server.
#[derive(Clone)]
pub struct GracefulShutdown {
    active: Arc<AtomicUsize>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    shutdown_rx: watch::Receiver<bool>,
    grace_period: Duration,
}

impl GracefulShutdown {
    pub fn new(grace_period: Duration) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            shutdown_tx: Arc::new(shutdown_tx),
            shutdown_rx,
            grace_period,
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        info!("graceful shutdown initiated");
    }

    pub fn is_shutdown(&self) -> bool {
        *self.shutdown_rx.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.shutdown_rx.clone()
    }

    pub fn track_conn(&self) -> ConnectionGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            active: Arc::clone(&self.active),
        }
    }

    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub async fn drain(&self) -> bool {
        let deadline = tokio::time::Instant::now() + self.grace_period;

        loop {
            let active = self.active.load(Ordering::Relaxed);
            if active == 0 {
                info!("all connections drained");
                return true;
            }

            if tokio::time::Instant::now() >= deadline {
                warn!(
                    remaining = active,
                    "grace period expired, forcing shutdown of remaining connections"
                );
                return false;
            }

            debug!(active, "waiting for connections to drain...");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// RAII guard that decrements the active connection count on drop.
pub struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_and_drop() {
        let shutdown = GracefulShutdown::new(Duration::from_secs(5));
        assert_eq!(shutdown.active_connections(), 0);
        let guard_a = shutdown.track_conn();
        let guard_b = shutdown.track_conn();
        assert_eq!(shutdown.active_connections(), 2);
        drop(guard_a);
        assert_eq!(shutdown.active_connections(), 1);
        drop(guard_b);
        assert_eq!(shutdown.active_connections(), 0);
    }

    #[test]
    fn shutdown_signal() {
        let shutdown = GracefulShutdown::new(Duration::from_secs(5));
        assert!(!shutdown.is_shutdown());
        shutdown.shutdown();
        assert!(shutdown.is_shutdown());
    }

    #[tokio::test]
    async fn drain_with_no_connections() {
        let shutdown = GracefulShutdown::new(Duration::from_millis(100));
        assert!(shutdown.drain().await);
    }
}
