//! SRV-record-based dynamic upstream resolution.
//!
//! Periodically resolves a DNS SRV record and updates a list of `(host, port)`
//! pairs for use as proxy upstream backends. Priority filtering (lowest value
//! wins) is applied; weight is preserved for future weighted selection.

use std::time::Duration;

use tracing::{debug, warn};

use super::dns_upstream::DynamicBackends;
use super::upstream::Backend;

/// A shared, atomically-updated list of resolved SRV backends.
pub type SrvBackends = std::sync::Arc<DynamicBackends>;

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
        let backends: SrvBackends = std::sync::Arc::new(DynamicBackends::new());
        let store = std::sync::Arc::clone(&backends);
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
            // Collect (addr, priority, weight) tuples.
            let mut entries: Vec<(String, u16, u16)> = records
                .iter()
                .map(|r| {
                    let host = r.target().to_string().trim_end_matches('.').to_string();
                    (format!("{host}:{}", r.port()), r.priority(), r.weight())
                })
                .collect();

            // Keep only the highest-priority (lowest numeric value) records.
            if let Some(min_priority) = entries.iter().map(|e| e.1).min() {
                entries.retain(|e| e.1 == min_priority);
            }

            let mut resolved: Vec<Backend> = entries
                .into_iter()
                .map(|(addr, _, weight)| Backend {
                    addr,
                    weight: u32::from(weight).max(1),
                    activity_key: None,
                })
                .collect();
            resolved.sort_by(|a, b| a.addr.cmp(&b.addr));
            resolved.dedup_by(|a, b| a.addr == b.addr);

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
            store.store(resolved);
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
        let backends: SrvBackends = std::sync::Arc::new(DynamicBackends::new());
        let cloned = std::sync::Arc::clone(&backends);
        assert!(std::sync::Arc::ptr_eq(&backends, &cloned));
        assert!(backends.load().is_empty());
    }
}
