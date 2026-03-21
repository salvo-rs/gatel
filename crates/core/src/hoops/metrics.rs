//! Prometheus-style metrics collection middleware.
//!
//! Tracks request counts, durations, and active connections using lock-free
//! atomics and a `DashMap` for per-label counters. Exposes a
//! `render_prometheus()` method that produces Prometheus text-format output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};

/// Composite key for per-request counters: `(host, method, status)`.
type CounterKey = (String, String, u16);

/// Composite key for duration histograms: `(host, method)`.
type DurationKey = (String, String);

/// Shared metrics store. Safe to use from multiple tasks concurrently.
pub struct Metrics {
    /// `gatel_requests_total` — counter per (host, method, status).
    request_counts: DashMap<CounterKey, AtomicU64>,
    /// `gatel_request_duration_seconds` — cumulative duration per (host, method).
    request_duration_sum: DashMap<DurationKey, AtomicU64>,
    /// `gatel_request_duration_seconds_count` — request count per (host, method)
    /// (used together with sum to compute average).
    request_duration_count: DashMap<DurationKey, AtomicU64>,
    /// `gatel_active_connections` — gauge.
    active_connections: AtomicU64,
}

impl Metrics {
    /// Create a new, empty metrics store.
    pub fn new() -> Self {
        Self {
            request_counts: DashMap::new(),
            request_duration_sum: DashMap::new(),
            request_duration_count: DashMap::new(),
            active_connections: AtomicU64::new(0),
        }
    }

    /// Record a completed request.
    pub fn record_request(&self, host: &str, method: &str, status: u16, duration: Duration) {
        // Increment the counter for (host, method, status).
        let key: CounterKey = (host.to_string(), method.to_string(), status);
        self.request_counts
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);

        // Accumulate duration in microseconds (gives sub-ms precision without floats).
        let micros = duration.as_micros() as u64;
        let dur_key: DurationKey = (host.to_string(), method.to_string());
        self.request_duration_sum
            .entry(dur_key.clone())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(micros, Ordering::Relaxed);
        self.request_duration_count
            .entry(dur_key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the active-connections gauge.
    pub fn inc_active_connections(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the active-connections gauge.
    pub fn dec_active_connections(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get the current active-connections count.
    pub fn active_connections(&self) -> u64 {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        // -- gatel_requests_total --
        out.push_str("# HELP gatel_requests_total Total number of HTTP requests.\n");
        out.push_str("# TYPE gatel_requests_total counter\n");
        for entry in self.request_counts.iter() {
            let (host, method, status) = entry.key();
            let count = entry.value().load(Ordering::Relaxed);
            out.push_str(&format!(
                "gatel_requests_total{{host=\"{host}\",method=\"{method}\",status=\"{status}\"}} {count}\n"
            ));
        }

        // -- gatel_request_duration_seconds (summary-style: sum + count) --
        out.push_str(
            "# HELP gatel_request_duration_seconds Total request processing time in seconds.\n",
        );
        out.push_str("# TYPE gatel_request_duration_seconds histogram\n");
        for entry in self.request_duration_sum.iter() {
            let (host, method) = entry.key();
            let sum_micros = entry.value().load(Ordering::Relaxed);
            let sum_secs = sum_micros as f64 / 1_000_000.0;
            let count = self
                .request_duration_count
                .get(entry.key())
                .map(|e| e.value().load(Ordering::Relaxed))
                .unwrap_or(0);
            out.push_str(&format!(
                "gatel_request_duration_seconds_sum{{host=\"{host}\",method=\"{method}\"}} {sum_secs:.6}\n"
            ));
            out.push_str(&format!(
                "gatel_request_duration_seconds_count{{host=\"{host}\",method=\"{method}\"}} {count}\n"
            ));
        }

        // -- gatel_active_connections --
        out.push_str("# HELP gatel_active_connections Current number of active connections.\n");
        out.push_str("# TYPE gatel_active_connections gauge\n");
        let active = self.active_connections.load(Ordering::Relaxed);
        out.push_str(&format!("gatel_active_connections {active}\n"));

        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Middleware implementation
// ---------------------------------------------------------------------------

/// Middleware that records request metrics (counter, duration, active gauge).
///
/// It wraps every request, measuring elapsed time and recording the result
/// after the downstream handler completes.
pub struct MetricsHoop {
    metrics: std::sync::Arc<Metrics>,
}

impl MetricsHoop {
    pub fn new(metrics: std::sync::Arc<Metrics>) -> Self {
        Self { metrics }
    }
}

#[async_trait]
impl salvo::Handler for MetricsHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let method = req.method().to_string();
        let host = req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let start = Instant::now();
        self.metrics.inc_active_connections();

        ctrl.call_next(req, depot, res).await;

        let elapsed = start.elapsed();
        self.metrics.dec_active_connections();

        let status = res.status_code.map(|s| s.as_u16()).unwrap_or(200);

        self.metrics.record_request(&host, &method, status, elapsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_render() {
        let m = Metrics::new();
        m.record_request("example.com", "GET", 200, Duration::from_millis(50));
        m.record_request("example.com", "GET", 200, Duration::from_millis(30));
        m.record_request("example.com", "POST", 201, Duration::from_millis(100));
        m.inc_active_connections();
        m.inc_active_connections();
        m.dec_active_connections();

        let output = m.render_prometheus();

        assert!(output.contains("gatel_requests_total"));
        assert!(output.contains("gatel_request_duration_seconds"));
        assert!(output.contains("gatel_active_connections 1"));
    }
}
