//! Fixed-cardinality process-owned background service metrics.
use std::fmt::Write as _;

use crate::{Counter, Gauge};

#[derive(Clone, Copy, Debug)]
pub enum WorkerKind {
    Probe,
    Resolver,
    Census,
}

impl WorkerKind {
    pub const ALL: [Self; 3] = [Self::Probe, Self::Resolver, Self::Census];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Resolver => "resolver",
            Self::Census => "census",
        }
    }
}

#[derive(Debug, Default)]
pub struct Workers([WorkerMetrics; 3]);

#[derive(Debug, Default)]
pub struct WorkerMetrics {
    pub starts: Counter,
    pub panicked: Counter,
    pub returned: Counter,
    backoff: Gauge,
}

impl Workers {
    pub fn get(&self, kind: WorkerKind) -> &WorkerMetrics {
        &self.0[kind as usize]
    }

    pub(crate) fn append(&self, out: &mut String) {
        out.push_str("# HELP gatel_worker_starts_total Background child starts including initial starts.\n# TYPE gatel_worker_starts_total counter\n");
        for kind in WorkerKind::ALL {
            let _ = writeln!(
                out,
                "gatel_worker_starts_total{{service=\"{}\"}} {}",
                kind.as_str(),
                self.get(kind).starts.get()
            );
        }
        out.push_str("# HELP gatel_worker_exits_total Unexpected child exits eligible for restart, not completed restarts.\n# TYPE gatel_worker_exits_total counter\n");
        for kind in WorkerKind::ALL {
            let metrics = self.get(kind);
            for (reason, value) in [
                ("panic", metrics.panicked.get()),
                ("returned", metrics.returned.get()),
            ] {
                let _ = writeln!(
                    out,
                    "gatel_worker_exits_total{{service=\"{}\",reason=\"{reason}\"}} {value}",
                    kind.as_str()
                );
            }
        }
        out.push_str("# HELP gatel_workers_backoff Background services currently waiting to restart.\n# TYPE gatel_workers_backoff gauge\n");
        for kind in WorkerKind::ALL {
            let _ = writeln!(
                out,
                "gatel_workers_backoff{{service=\"{}\"}} {}",
                kind.as_str(),
                self.get(kind).backoff.get()
            );
        }
    }
}

impl WorkerMetrics {
    pub fn enter_backoff(&self) -> Backoff<'_> {
        self.backoff.increment();
        Backoff(&self.backoff)
    }
}

/// Keeps the gauge correct when the parent future is cancelled or aborted.
pub struct Backoff<'a>(&'a Gauge);
impl Drop for Backoff<'_> {
    fn drop(&mut self) {
        self.0.decrement();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_metrics_have_twelve_fixed_series_and_backoff_is_scoped() {
        let telemetry = crate::Telemetry::new();
        let metrics = telemetry.workers.get(WorkerKind::Probe);
        metrics.starts.increment();
        metrics.panicked.increment();
        let first = metrics.enter_backoff();
        let second = metrics.enter_backoff();
        assert!(crate::prometheus(&telemetry).contains("gatel_workers_backoff{service=\"probe\"} 2\n"));
        drop(first);
        drop(second);
        let rendered = crate::prometheus(&telemetry);
        assert!(rendered.contains("gatel_workers_backoff{service=\"probe\"} 0\n"));
        assert!(rendered.contains("gatel_worker_exits_total{service=\"probe\",reason=\"panic\"} 1\n"));
        let lines: Vec<_> = rendered
            .lines()
            .filter(|line| line.starts_with("gatel_worker"))
            .collect();
        assert_eq!(lines.len(), 12);
        assert_eq!(telemetry.metrics.series(), 0, "no dynamic series registration");
        assert_eq!(rendered, crate::prometheus(&telemetry));
    }
}
