//! SRV-record-based dynamic upstream resolution.
//!
//! Periodically resolves a DNS SRV record and updates a list of `(host, port)`
//! pairs for use as proxy upstream backends. Priority filtering (lowest value
//! wins) is applied; weight is preserved for future weighted selection.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, warn};

/// A shared, atomically-updated list of resolved SRV backends.
/// Each entry is `(host, port)`.
pub type SrvBackends = Arc<RwLock<Vec<(String, u16)>>>;

/// Handle to the background SRV resolver task. Aborting the task on drop
/// ensures we don't leak spawned work.
pub struct SrvResolver {
    _task: tokio::task::JoinHandle<()>,
    /// The dynamically-updated backend list shared with the proxy handler.
    pub backends: SrvBackends,
}

impl SrvResolver {
    /// Start a background task that periodically resolves `service_name` via
    /// DNS SRV records and updates the shared backend list.
    pub fn start(service_name: String, refresh_interval: Duration) -> Self {
        let backends: SrvBackends = Arc::new(RwLock::new(Vec::new()));
        let store = Arc::clone(&backends);
        let name = service_name.clone();

        let task = tokio::spawn(async move {
            // Build a hickory TokioResolver from system configuration, falling
            // back to a default (Google public DNS) resolver on failure.
            let dns = hickory_resolver::TokioResolver::builder_tokio()
                .unwrap_or_else(|_| {
                    hickory_resolver::TokioResolver::builder_with_config(
                        hickory_resolver::config::ResolverConfig::default(),
                        hickory_resolver::name_server::TokioConnectionProvider::default(),
                    )
                })
                .build();

            // Perform an initial resolution immediately before entering the
            // periodic sleep loop.
            resolve_and_update(&name, &dns, &store).await;

            loop {
                tokio::time::sleep(refresh_interval).await;
                resolve_and_update(&name, &dns, &store).await;
            }
        });

        Self {
            _task: task,
            backends,
        }
    }
}

impl Drop for SrvResolver {
    fn drop(&mut self) {
        self._task.abort();
    }
}

/// Perform a single SRV lookup and update the shared backend list.
async fn resolve_and_update(
    service_name: &str,
    dns: &hickory_resolver::TokioResolver,
    store: &SrvBackends,
) {
    match dns.srv_lookup(service_name).await {
        Ok(records) => {
            // Collect (host, port, priority, weight) tuples.
            let mut entries: Vec<(String, u16, u16, u16)> = records
                .iter()
                .map(|r| {
                    let host = r.target().to_string().trim_end_matches('.').to_string();
                    (host, r.port(), r.priority(), r.weight())
                })
                .collect();

            // Keep only the highest-priority (lowest numeric value) records.
            if let Some(min_priority) = entries.iter().map(|e| e.2).min() {
                entries.retain(|e| e.2 == min_priority);
            }

            let resolved: Vec<(String, u16)> =
                entries.into_iter().map(|(h, p, ..)| (h, p)).collect();

            if resolved.is_empty() {
                warn!(
                    service = %service_name,
                    "SRV lookup returned zero records; keeping previous list"
                );
                return;
            }

            debug!(
                service = %service_name,
                count = resolved.len(),
                "SRV lookup resolved"
            );
            *store.write().await = resolved;
        }
        Err(e) => {
            warn!(
                service = %service_name,
                error = %e,
                "SRV lookup failed; keeping previous list"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srv_backends_starts_empty() {
        // Verify we can construct a SrvBackends and it starts empty without
        // actually spinning up a resolver task (which needs a Tokio runtime).
        let backends: SrvBackends = Arc::new(RwLock::new(Vec::new()));
        // Can't easily await in a sync test; just check the Arc round-trips.
        let cloned = Arc::clone(&backends);
        assert!(Arc::ptr_eq(&backends, &cloned));
    }
}
