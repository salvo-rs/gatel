use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::upstream::UpstreamPool;
use crate::config::{HealthCheckConfig, PassiveHealthConfig};

// ---------------------------------------------------------------------------
// Active health checker
// ---------------------------------------------------------------------------

/// Performs periodic HTTP health checks against every backend in the pool and
/// updates the pool's `healthy` flags accordingly.
pub struct HealthChecker {
    /// Handle to the spawned background task so we can abort on drop.
    _task: tokio::task::JoinHandle<()>,
}

impl HealthChecker {
    /// Spawn an active health-check loop.  Returns immediately; the checks
    /// run on a background Tokio task.
    pub fn start(pool: Arc<UpstreamPool>, config: &HealthCheckConfig) -> Self {
        let uri = config.uri.clone();
        let interval = config.interval;
        let timeout = config.timeout;
        let unhealthy_threshold = config.unhealthy_threshold;
        let healthy_threshold = config.healthy_threshold;
        let n = pool.len();

        let task = tokio::spawn(async move {
            // Per-backend consecutive success / failure counters.
            let mut consecutive_ok: Vec<u32> = vec![0; n];
            let mut consecutive_fail: Vec<u32> = vec![0; n];

            // Build a lightweight HTTP client for probes.
            let client =
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build_http::<crate::Body>();

            loop {
                for idx in 0..n {
                    let addr = &pool.backends[idx].addr;
                    let check_uri = format!("http://{}{}", addr, uri);

                    let result = tokio::time::timeout(timeout, async {
                        let req = http::Request::builder()
                            .method(http::Method::GET)
                            .uri(&check_uri)
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

                    match result {
                        Ok(Ok(())) => {
                            consecutive_fail[idx] = 0;
                            consecutive_ok[idx] += 1;
                            if consecutive_ok[idx] >= healthy_threshold {
                                if !pool.is_healthy(idx) {
                                    debug!(backend = addr, "active health check: marking healthy");
                                    pool.set_healthy(idx, true);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            consecutive_ok[idx] = 0;
                            consecutive_fail[idx] += 1;
                            if consecutive_fail[idx] >= unhealthy_threshold {
                                if pool.is_healthy(idx) {
                                    warn!(
                                        backend = addr,
                                        error = %e,
                                        "active health check: marking unhealthy"
                                    );
                                    pool.set_healthy(idx, false);
                                }
                            }
                        }
                        Err(_elapsed) => {
                            consecutive_ok[idx] = 0;
                            consecutive_fail[idx] += 1;
                            if consecutive_fail[idx] >= unhealthy_threshold {
                                if pool.is_healthy(idx) {
                                    warn!(
                                        backend = addr,
                                        "active health check: marking unhealthy (timeout)"
                                    );
                                    pool.set_healthy(idx, false);
                                }
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

// ---------------------------------------------------------------------------
// Passive health checker
// ---------------------------------------------------------------------------

/// Tracks 5xx responses per backend and temporarily marks backends unhealthy
/// when they exceed a failure threshold within a sliding time window.
/// After a cooldown period the backend is automatically re-enabled.
pub struct PassiveHealthChecker {
    entries: Vec<PassiveEntry>,
    config: PassiveHealthConfig,
}

struct PassiveEntry {
    /// Ring buffer of timestamps (as millis since an arbitrary epoch) of
    /// recent failures.  Protected by a mutex because we occasionally compact.
    failures: Mutex<Vec<Instant>>,
    /// Whether the backend has been passively disabled.
    disabled: AtomicBool,
    /// Timestamp (millis since epoch) when the backend was disabled.
    disabled_at: Mutex<Option<Instant>>,
}

impl PassiveHealthChecker {
    /// Create a new passive health tracker for `n` backends.
    pub fn new(n: usize, config: &PassiveHealthConfig) -> Self {
        let entries = (0..n)
            .map(|_| PassiveEntry {
                failures: Mutex::new(Vec::new()),
                disabled: AtomicBool::new(false),
                disabled_at: Mutex::new(None),
            })
            .collect();
        Self {
            entries,
            config: config.clone(),
        }
    }

    /// Record a 5xx failure for backend `idx`.  If the number of failures in
    /// the configured window exceeds `max_fails`, the backend is disabled.
    pub async fn record_failure(&self, idx: usize, pool: &UpstreamPool) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let now = Instant::now();
        let window = self.config.fail_window;

        let mut failures = entry.failures.lock().await;
        failures.push(now);
        // Remove failures outside the window.
        failures.retain(|&t| now.duration_since(t) < window);

        if failures.len() as u32 >= self.config.max_fails {
            if !entry.disabled.swap(true, Ordering::Relaxed) {
                warn!(
                    backend = pool.backends[idx].addr,
                    fails = failures.len(),
                    "passive health: disabling backend"
                );
                pool.set_healthy(idx, false);
                *entry.disabled_at.lock().await = Some(now);
            }
        }
    }

    /// Check whether any previously-disabled backend should be re-enabled
    /// after the cooldown period.  Call this periodically (e.g. after each
    /// request or on a timer).
    pub async fn maybe_recover(&self, pool: &UpstreamPool) {
        let cooldown = self.config.cooldown;
        let now = Instant::now();

        for (idx, entry) in self.entries.iter().enumerate() {
            if !entry.disabled.load(Ordering::Relaxed) {
                continue;
            }
            let disabled_at = *entry.disabled_at.lock().await;
            if let Some(at) = disabled_at {
                if now.duration_since(at) >= cooldown {
                    debug!(
                        backend = pool.backends[idx].addr,
                        "passive health: re-enabling backend after cooldown"
                    );
                    entry.disabled.store(false, Ordering::Relaxed);
                    pool.set_healthy(idx, true);
                    // Reset failure history so we start fresh.
                    entry.failures.lock().await.clear();
                    *entry.disabled_at.lock().await = None;
                }
            }
        }
    }

    /// Returns `true` if the backend is currently passively disabled.
    pub fn is_disabled(&self, idx: usize) -> bool {
        self.entries
            .get(idx)
            .map(|e| e.disabled.load(Ordering::Relaxed))
            .unwrap_or(false)
    }
}
