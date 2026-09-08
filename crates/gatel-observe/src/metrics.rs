//! What the process measures, and the handles it hands out.
//!
//! Every label value here comes from the plan — a site name, a route name, an
//! upstream name, a listener name — and every handle is resolved during prepare.
//! Nothing on the request path names a series, which is why the label set stays
//! the size of the configuration however much traffic arrives.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::instrument::{Counter, Gauge, HighWaterMark, Histogram};
use crate::series::{Labels, Series};

/// The class of a response status.
///
/// Not the exact code: a backend chooses that, and a backend under attack can
/// choose a great many of them. The class is enough to alert on and cannot be
/// made to grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Other,
}

impl Class {
    pub const ALL: [Class; 6] = [
        Class::Informational,
        Class::Success,
        Class::Redirect,
        Class::ClientError,
        Class::ServerError,
        Class::Other,
    ];

    pub const fn of(status: u16) -> Self {
        match status {
            100..=199 => Class::Informational,
            200..=299 => Class::Success,
            300..=399 => Class::Redirect,
            400..=499 => Class::ClientError,
            500..=599 => Class::ServerError,
            _ => Class::Other,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Informational => "1xx",
            Class::Success => "2xx",
            Class::Redirect => "3xx",
            Class::ClientError => "4xx",
            Class::ServerError => "5xx",
            Class::Other => "other",
        }
    }
}

/// How a configuration transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reload {
    Started,
    Reloaded,
    NoOp,
    Rejected,
    PrepareFailed,
    Stale,
}

impl Reload {
    pub const ALL: [Reload; 6] = [
        Reload::Started,
        Reload::Reloaded,
        Reload::NoOp,
        Reload::Rejected,
        Reload::PrepareFailed,
        Reload::Stale,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Reload::Started => "started",
            Reload::Reloaded => "reloaded",
            Reload::NoOp => "no_op",
            Reload::Rejected => "rejected",
            Reload::PrepareFailed => "prepare_failed",
            Reload::Stale => "stale",
        }
    }
}

/// How one attempt against a backend ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// A response head came back, whatever it said.
    Answered,
    /// The connection failed, or the backend spoke nonsense.
    Failed,
    /// A deadline passed first.
    TimedOut,
    /// No endpoint could be picked at all — every one ejected, or the pool full.
    Unavailable,
}

impl Attempt {
    pub const ALL: [Attempt; 4] = [
        Attempt::Answered,
        Attempt::Failed,
        Attempt::TimedOut,
        Attempt::Unavailable,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Attempt::Answered => "answered",
            Attempt::Failed => "failed",
            Attempt::TimedOut => "timed_out",
            Attempt::Unavailable => "unavailable",
        }
    }
}

/// The condition of an endpoint, as a gauge dimension.
///
/// The same four names the runtime uses, so a dashboard and a log line agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Configured, never probed.
    Unknown,
    Healthy,
    Unhealthy,
    /// Taken out deliberately.
    Draining,
}

impl Condition {
    pub const ALL: [Condition; 4] = [
        Condition::Unknown,
        Condition::Healthy,
        Condition::Unhealthy,
        Condition::Draining,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Condition::Unknown => "unknown",
            Condition::Healthy => "healthy",
            Condition::Unhealthy => "unhealthy",
            Condition::Draining => "draining",
        }
    }
}

/// Counters and gauges that belong to the process rather than to any resource.
#[derive(Debug, Default)]
pub struct Process {
    /// One store/boot per Process: do not reuse this watermark for a new store.
    pub generation: HighWaterMark,
    reloads: [Counter; Reload::ALL.len()],
    /// Unix seconds. Zero until a configuration has been accepted, which is how
    /// an alert tells "never started" from "has not reloaded lately".
    pub last_reload_success: Gauge,
    pub start_timestamp: Gauge,
}

impl Process {
    pub fn reloaded(&self, outcome: Reload) {
        self.reloads[outcome as usize].increment();
        if matches!(outcome, Reload::Started | Reload::Reloaded | Reload::NoOp) {
            self.last_reload_success.set(unix_seconds());
        }
    }

    pub fn reloads(&self, outcome: Reload) -> u64 {
        self.reloads[outcome as usize].get()
    }
}

/// Per-listener transport counters. Labels: `listener`.
#[derive(Debug, Default)]
pub struct Listener {
    pub connections_total: Counter,
    pub connections_active: Gauge,
    pub handshakes_ok: Counter,
    pub handshakes_failed: Counter,
    /// Requests no route claimed. Counted here rather than per route, because
    /// there is no route to attribute them to and a path is not a label.
    pub unmatched_requests: Counter,
}

/// Per-route request counters. Labels: `site`, `route`.
#[derive(Debug, Default)]
pub struct Route {
    requests: [Counter; Class::ALL.len()],
    pub duration: Histogram,
    pub response_bytes: Counter,
    pub in_flight: Gauge,
}

impl Route {
    /// Record one finished request.
    ///
    /// Called when the body ended, not when the handler returned, so a
    /// streaming response is not reported as instant.
    pub fn completed(&self, status: u16, elapsed: Duration, bytes: u64) {
        self.requests[Class::of(status) as usize].increment();
        self.duration.record(elapsed);
        self.response_bytes.add(bytes);
    }

    pub fn requests(&self, class: Class) -> u64 {
        self.requests[class as usize].get()
    }
}

/// Per-upstream backend counters. Labels: `upstream`.
#[derive(Debug, Default)]
pub struct Upstream {
    attempts: [Counter; Attempt::ALL.len()],
    pub duration: Histogram,
    endpoints: [Gauge; Condition::ALL.len()],
    pub pool_idle: Gauge,
    pub pool_in_use: Gauge,
}

impl Upstream {
    pub fn attempted(&self, outcome: Attempt, elapsed: Duration) {
        self.attempts[outcome as usize].increment();
        self.duration.record(elapsed);
    }

    pub fn attempts(&self, outcome: Attempt) -> u64 {
        self.attempts[outcome as usize].get()
    }

    /// Publish the endpoint census after a probe round.
    pub fn census(&self, condition: Condition, count: i64) {
        self.endpoints[condition as usize].set(count);
    }

    pub fn endpoints(&self, condition: Condition) -> i64 {
        self.endpoints[condition as usize].get()
    }
}

/// The label sets a plan still names, so the rest can be forgotten.
#[derive(Debug, Default)]
pub struct Live {
    pub listeners: HashSet<Labels>,
    pub routes: HashSet<Labels>,
    pub upstreams: HashSet<Labels>,
}

impl Live {
    pub fn listener(&mut self, name: &str) {
        self.listeners.insert(vec![name.to_string()]);
    }

    pub fn route(&mut self, site: &str, route: &str) {
        self.routes.insert(vec![site.to_string(), route.to_string()]);
    }

    pub fn upstream(&mut self, name: &str) {
        self.upstreams.insert(vec![name.to_string()]);
    }
}

/// Everything the process measures.
#[derive(Debug)]
pub struct Metrics {
    pub process: Process,
    pub(crate) listeners: Series<Listener>,
    pub(crate) routes: Series<Route>,
    pub(crate) upstreams: Series<Upstream>,
    pub(crate) version: String,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let process = Process::default();
        process.start_timestamp.set(unix_seconds());
        Self {
            process,
            listeners: Series::new(),
            routes: Series::new(),
            upstreams: Series::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn listener(&self, name: &str) -> Arc<Listener> {
        self.listeners.resolve(vec![name.to_string()])
    }

    pub fn route(&self, site: &str, route: &str) -> Arc<Route> {
        self.routes.resolve(vec![site.to_string(), route.to_string()])
    }

    pub fn upstream(&self, name: &str) -> Arc<Upstream> {
        self.upstreams.resolve(vec![name.to_string()])
    }

    /// Drop every series the new plan no longer names.
    pub fn retain(&self, live: &Live) {
        self.listeners.retain(&live.listeners);
        self.routes.retain(&live.routes);
        self.upstreams.retain(&live.upstreams);
    }

    /// How many series exist, across every family.
    pub fn series(&self) -> usize {
        self.listeners.len() + self.routes.len() + self.upstreams.len()
    }
}

pub(crate) fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_becomes_one_of_six_classes() {
        assert_eq!(Class::of(204), Class::Success);
        assert_eq!(Class::of(404), Class::ClientError);
        assert_eq!(Class::of(503), Class::ServerError);
        // A backend can invent this one; it must not invent a label.
        assert_eq!(Class::of(999), Class::Other);
    }

    #[test]
    fn traffic_cannot_create_a_series() {
        let metrics = Metrics::new();
        let route = metrics.route("*", "api");
        assert_eq!(metrics.series(), 1);
        // Whatever a client sends, all it can reach is the handle above.
        for status in 100..600 {
            route.completed(status, Duration::from_millis(1), 10);
        }
        assert_eq!(metrics.series(), 1, "500 distinct statuses, still one series");
        assert_eq!(route.requests(Class::ServerError), 100);
    }

    #[test]
    fn a_reload_keeps_the_counts_a_route_already_had() {
        let metrics = Metrics::new();
        metrics.route("*", "api").completed(200, Duration::ZERO, 0);
        // The next generation resolves the same name.
        assert_eq!(metrics.route("*", "api").requests(Class::Success), 1);
    }

    #[test]
    fn a_deleted_route_is_forgotten() {
        let metrics = Metrics::new();
        metrics.route("*", "api");
        metrics.route("*", "web");
        metrics.upstream("api");
        let mut live = Live::default();
        live.route("*", "api");
        live.upstream("api");
        metrics.retain(&live);
        assert_eq!(metrics.series(), 2);
    }

    #[test]
    fn only_an_accepted_configuration_moves_the_success_timestamp() {
        let metrics = Metrics::new();
        assert_eq!(metrics.process.last_reload_success.get(), 0);
        metrics.process.reloaded(Reload::Rejected);
        assert_eq!(
            metrics.process.last_reload_success.get(),
            0,
            "a rejected configuration is not a successful reload"
        );
        assert_eq!(metrics.process.reloads(Reload::Rejected), 1);
        metrics.process.reloaded(Reload::Reloaded);
        assert!(metrics.process.last_reload_success.get() > 0);
    }

    #[test]
    fn outcome_names_are_stable_and_distinct() {
        let names: HashSet<_> = Reload::ALL.iter().map(|outcome| outcome.as_str()).collect();
        assert_eq!(names.len(), Reload::ALL.len());
        let names: HashSet<_> = Attempt::ALL.iter().map(|outcome| outcome.as_str()).collect();
        assert_eq!(names.len(), Attempt::ALL.len());
        // The discriminant indexes an array, so the order must match.
        for (index, outcome) in Reload::ALL.iter().enumerate() {
            assert_eq!(index, *outcome as usize);
        }
        for (index, condition) in Condition::ALL.iter().enumerate() {
            assert_eq!(index, *condition as usize);
        }
    }
}
