use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::upstream::{BackendEntry, BackendSnapshot, UpstreamPool};
use crate::config::{HealthCheckConfig, PassiveHealthConfig};

// ---------------------------------------------------------------------------
// Active health checker
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HealthCheckCounters {
    consecutive_ok: u32,
    consecutive_fail: u32,
}

/// Performs periodic HTTP health checks against every current backend snapshot
/// and updates each backend's shared `healthy` flag.
pub struct HealthChecker {
    /// Handle to the spawned background task so we can abort on drop.
    _task: tokio::task::JoinHandle<()>,
}

impl HealthChecker {
    /// Spawn an active health-check loop. Returns immediately; the checks run
    /// on a background Tokio task.
    pub fn start(pool: Arc<UpstreamPool>, config: &HealthCheckConfig) -> Self {
        let uri = config.uri.clone();
        let interval = config.interval;
        let timeout = config.timeout;
        let unhealthy_threshold = config.unhealthy_threshold;
        let healthy_threshold = config.healthy_threshold;

        let task = tokio::spawn(async move {
            // Consecutive success/failure counters are keyed by backend address
            // so dynamic DNS/SRV updates cannot accidentally inherit state by index.
            let mut counters: HashMap<String, HealthCheckCounters> = HashMap::new();

            // Build a lightweight HTTP client for probes.
            let client =
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build_http::<crate::Body>();

            loop {
                let snapshot = pool.snapshot();
                let active_addrs: HashSet<String> = snapshot
                    .entries()
                    .iter()
                    .map(|entry| entry.backend.addr.clone())
                    .collect();
                counters.retain(|addr, _| active_addrs.contains(addr));

                for backend in snapshot.entries().iter().cloned() {
                    let addr = backend.backend.addr.clone();
                    let check_uri = health_check_uri(&addr, &uri);

                    let result = tokio::time::timeout(timeout, async {
                        let req = http::Request::builder()
                            .method(http::Method::GET)
                            .uri(check_uri.as_str())
                            .body(crate::empty_body())
                            .map_err(|e| e.to_string())?;

                        let resp = client.request(req).await.map_err(|e| e.to_string())?;

                        if resp.status().is_success() {
                            Ok(())
                        } else {
                            Err(format!("status {}", resp.status()))
                        }
                    })
                    .await;

                    let counts = counters.entry(addr.clone()).or_default();

                    match result {
                        Ok(Ok(())) => {
                            counts.consecutive_fail = 0;
                            counts.consecutive_ok += 1;
                            if counts.consecutive_ok >= healthy_threshold && !backend.is_healthy() {
                                debug!(backend = %addr, "active health check: marking healthy");
                                backend.set_healthy(true);
                            }
                        }
                        Ok(Err(e)) => {
                            counts.consecutive_ok = 0;
                            counts.consecutive_fail += 1;
                            if counts.consecutive_fail >= unhealthy_threshold
                                && backend.is_healthy()
                            {
                                warn!(
                                    backend = %addr,
                                    error = %e,
                                    "active health check: marking unhealthy"
                                );
                                backend.set_healthy(false);
                            }
                        }
                        Err(_elapsed) => {
                            counts.consecutive_ok = 0;
                            counts.consecutive_fail += 1;
                            if counts.consecutive_fail >= unhealthy_threshold
                                && backend.is_healthy()
                            {
                                warn!(
                                    backend = %addr,
                                    "active health check: marking unhealthy (timeout)"
                                );
                                backend.set_healthy(false);
                            }
                        }
                    }
                }

                tokio::time::sleep(interval).await;
            }
        });

        Self { _task: task }
    }
}

impl Drop for HealthChecker {
    fn drop(&mut self) {
        self._task.abort();
    }
}

fn health_check_uri(addr: &str, uri: &str) -> String {
    let scheme = if addr.starts_with("http://") || addr.starts_with("https://") {
        ""
    } else {
        "http://"
    };
    let separator = if uri.starts_with('/') { "" } else { "/" };
    format!("{scheme}{addr}{separator}{uri}")
}

// ---------------------------------------------------------------------------
// Passive health checker
// ---------------------------------------------------------------------------

/// Tracks failures per backend address and temporarily marks backends unhealthy
/// when they exceed a failure threshold within a sliding time window. After a
/// cooldown period the backend is automatically re-enabled.
pub struct PassiveHealthChecker {
    entries: DashMap<String, Arc<PassiveEntry>>,
    config: PassiveHealthConfig,
}

struct PassiveEntry {
    /// Recent failure timestamps. Protected by a mutex because we occasionally
    /// compact the list.
    failures: Mutex<Vec<Instant>>,
    /// Whether the backend has been passively disabled.
    disabled: AtomicBool,
    /// Timestamp when the backend was disabled.
    disabled_at: Mutex<Option<Instant>>,
}

impl PassiveEntry {
    fn new() -> Self {
        Self {
            failures: Mutex::new(Vec::new()),
            disabled: AtomicBool::new(false),
            disabled_at: Mutex::new(None),
        }
    }
}

impl PassiveHealthChecker {
    /// Create a new passive health tracker. The backend count is accepted for
    /// compatibility with older call sites; dynamic backends are tracked by
    /// address as they appear in snapshots.
    pub fn new(_n: usize, config: &PassiveHealthConfig) -> Self {
        Self {
            entries: DashMap::new(),
            config: config.clone(),
        }
    }

    fn entry_for(&self, addr: &str) -> Arc<PassiveEntry> {
        Arc::clone(
            self.entries
                .entry(addr.to_string())
                .or_insert_with(|| Arc::new(PassiveEntry::new()))
                .value(),
        )
    }

    /// Record a failure for `backend`. If the number of failures in the
    /// configured window exceeds `max_fails`, the backend is disabled.
    pub async fn record_failure(&self, backend: &BackendEntry) {
        let addr = backend.backend.addr.clone();
        let entry = self.entry_for(&addr);
        let now = Instant::now();
        let window = self.config.fail_window;

        let mut failures = entry.failures.lock().await;
        failures.push(now);
        failures.retain(|&t| now.duration_since(t) < window);

        if failures.len() as u32 >= self.config.max_fails
            && !entry.disabled.swap(true, Ordering::Relaxed)
        {
            warn!(
                backend = %addr,
                fails = failures.len(),
                "passive health: disabling backend"
            );
            backend.set_healthy(false);
            *entry.disabled_at.lock().await = Some(now);
        }
    }

    /// Check whether any previously-disabled backend should be re-enabled
    /// after the cooldown period.
    pub async fn maybe_recover(&self, snapshot: &BackendSnapshot) {
        let cooldown = self.config.cooldown;
        let now = Instant::now();
        let active_addrs: HashSet<String> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.backend.addr.clone())
            .collect();
        self.entries.retain(|addr, _| active_addrs.contains(addr));

        for backend in snapshot.entries().iter().cloned() {
            let addr = backend.backend.addr.clone();
            let Some(entry) = self
                .entries
                .get(&addr)
                .map(|entry| Arc::clone(entry.value()))
            else {
                continue;
            };
            if !entry.disabled.load(Ordering::Relaxed) {
                continue;
            }
            let disabled_at = *entry.disabled_at.lock().await;
            if let Some(at) = disabled_at
                && now.duration_since(at) >= cooldown
            {
                debug!(
                    backend = %addr,
                    "passive health: re-enabling backend after cooldown"
                );
                entry.disabled.store(false, Ordering::Relaxed);
                backend.set_healthy(true);
                entry.failures.lock().await.clear();
                *entry.disabled_at.lock().await = None;
            }
        }
    }

    /// Returns `true` if the backend address is currently passively disabled.
    pub fn is_disabled(&self, addr: &str) -> bool {
        self.entries
            .get(addr)
            .map(|e| e.disabled.load(Ordering::Relaxed))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::config::{LbPolicy, ProxyConfig, UpstreamConfig};

    fn proxy_config() -> ProxyConfig {
        ProxyConfig {
            upstreams: vec![UpstreamConfig {
                addr: "127.0.0.1:3000".to_string(),
                weight: 1,
                activity_key: None,
            }],
            lb: LbPolicy::RoundRobin,
            lb_header: None,
            lb_cookie: None,
            health_check: None,
            passive_health: None,
            headers_up: HashMap::new(),
            headers_down: HashMap::new(),
            retries: 0,
            retry_buffer_limit: crate::config::DEFAULT_RETRY_BUFFER_LIMIT,
            dynamic_upstreams: None,
            error_pages: HashMap::new(),
            headers_up_replace: Vec::new(),
            tls_skip_verify: false,
            upstream_http2: false,
            max_connections: None,
            keepalive_timeout: None,
            sanitize_uri: true,
            srv_upstream: None,
        }
    }

    #[test]
    fn health_check_uri_preserves_explicit_backend_scheme() {
        assert_eq!(
            health_check_uri("https://example.com", "/health"),
            "https://example.com/health"
        );
        assert_eq!(
            health_check_uri("127.0.0.1:3000", "health"),
            "http://127.0.0.1:3000/health"
        );
    }

    #[tokio::test]
    async fn passive_health_tracks_backends_by_address() {
        let pool = UpstreamPool::from_config(&proxy_config());
        let snapshot = pool.snapshot();
        let backend = snapshot.get(0).unwrap().clone();
        let checker = PassiveHealthChecker::new(
            snapshot.len(),
            &PassiveHealthConfig {
                max_fails: 1,
                fail_window: Duration::from_secs(30),
                cooldown: Duration::from_secs(60),
            },
        );

        checker.record_failure(&backend).await;

        assert!(checker.is_disabled("127.0.0.1:3000"));
        assert!(!pool.snapshot().is_healthy(0));
    }
}
