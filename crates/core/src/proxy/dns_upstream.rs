//! DNS-based dynamic upstream resolution.
//!
//! Periodically resolves a DNS name and updates an `UpstreamPool` with the
//! resulting addresses.  Currently supports A/AAAA records via
//! `tokio::net::lookup_host`; SRV record support is planned as a future
//! extension.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use arc_swap::ArcSwap;
use tracing::{debug, info, warn};

use super::upstream::Backend;

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

/// Which DNS record types to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsRecordType {
    /// A and AAAA records.  The port is taken from configuration.
    A,
    /// SRV records (future extension).  Port comes from the SRV record.
    SRV,
}

/// Configuration for a DNS-based dynamic upstream source.
#[derive(Debug, Clone)]
pub struct DynamicUpstreamConfig {
    /// DNS name to resolve, e.g. `"app.svc.cluster.local"`.
    pub dns_name: String,
    /// Port to pair with resolved IPs (used for A/AAAA; ignored for SRV).
    pub port: u16,
    /// How often to re-resolve.
    pub refresh_interval: Duration,
}

// ---------------------------------------------------------------------------
// Dynamic backend list
// ---------------------------------------------------------------------------

/// A thread-safe, atomically swappable list of backends populated by DNS
/// resolution.  Readers get a snapshot via `load()`; the background resolver
/// updates via `store()`.
pub struct DynamicBackends {
    inner: ArcSwap<Vec<Backend>>,
}

impl Default for DynamicBackends {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicBackends {
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Get the current list of backends.
    pub fn load(&self) -> arc_swap::Guard<Arc<Vec<Backend>>> {
        self.inner.load()
    }

    /// Replace the backend list atomically.
    pub fn store(&self, backends: Vec<Backend>) {
        self.inner.store(Arc::new(backends));
    }
}

// ---------------------------------------------------------------------------
// DNS resolver task
// ---------------------------------------------------------------------------

/// Handle to the background DNS resolver.  Aborting the task on drop ensures
/// we don't leak spawned work.
pub struct DnsResolver {
    _task: tokio::task::JoinHandle<()>,
    /// The dynamically-updated backend list.  Shared with whoever needs to
    /// read the current set of upstreams.
    pub backends: Arc<DynamicBackends>,
}

impl DnsResolver {
    /// Start a background task that periodically resolves `config.dns_name`
    /// and updates the shared backend list.
    pub fn start(config: &DynamicUpstreamConfig) -> Self {
        let backends = Arc::new(DynamicBackends::new());
        let backends_ref = Arc::clone(&backends);
        let dns_name = config.dns_name.clone();
        let port = config.port;
        let interval = config.refresh_interval;

        let task = tokio::spawn(async move {
            // Perform an initial resolution immediately.
            resolve_and_update(&dns_name, port, &backends_ref).await;

            loop {
                tokio::time::sleep(interval).await;
                resolve_and_update(&dns_name, port, &backends_ref).await;
            }
        });

        Self {
            _task: task,
            backends,
        }
    }
}

impl Drop for DnsResolver {
    fn drop(&mut self) {
        self._task.abort();
    }
}

/// Resolve a DNS name and update the dynamic backend list.
async fn resolve_and_update(dns_name: &str, port: u16, backends: &DynamicBackends) {
    let lookup_target = format!("{dns_name}:{port}");

    match tokio::net::lookup_host(&lookup_target).await {
        Ok(addrs) => {
            let mut new_backends: Vec<Backend> = addrs
                .map(|addr| Backend {
                    addr: addr.to_string(),
                    weight: 1,
                })
                .collect();

            // Sort for deterministic ordering so we can detect changes.
            new_backends.sort_by(|a, b| a.addr.cmp(&b.addr));
            // Deduplicate.
            new_backends.dedup_by(|a, b| a.addr == b.addr);

            let count = new_backends.len();
            debug!(
                dns = dns_name,
                resolved = count,
                "DNS upstream resolution complete"
            );

            if new_backends.is_empty() {
                warn!(
                    dns = dns_name,
                    "DNS resolution returned zero addresses; keeping previous list"
                );
                return;
            }

            backends.store(new_backends);
            info!(
                dns = dns_name,
                backends = count,
                "updated dynamic upstream backends"
            );
        }
        Err(e) => {
            warn!(
                dns = dns_name,
                error = %e,
                "DNS resolution failed; keeping previous list"
            );
        }
    };
}

/// Build per-backend health and connection tracking vectors for a set of
/// dynamic backends.  This is a helper for code that needs to construct an
/// `UpstreamPool`-like structure from dynamic backends.
pub fn build_tracking_vecs(count: usize) -> (Vec<AtomicBool>, Vec<AtomicUsize>) {
    let healthy: Vec<AtomicBool> = (0..count).map(|_| AtomicBool::new(true)).collect();
    let active_conns: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
    (healthy, active_conns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_backends_store_load() {
        let db = DynamicBackends::new();
        assert!(db.load().is_empty());

        db.store(vec![
            Backend {
                addr: "1.2.3.4:8080".into(),
                weight: 1,
            },
            Backend {
                addr: "5.6.7.8:8080".into(),
                weight: 1,
            },
        ]);
        let loaded = db.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].addr, "1.2.3.4:8080");
    }

    #[test]
    fn test_build_tracking_vecs() {
        let (healthy, conns) = build_tracking_vecs(3);
        assert_eq!(healthy.len(), 3);
        assert_eq!(conns.len(), 3);
        assert!(healthy[0].load(std::sync::atomic::Ordering::Relaxed));
    }
}
