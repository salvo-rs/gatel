//! Readiness that says which of several different things is wrong.
//!
//! The distinction that matters is between a condition local to this process
//! and one shared by every copy of it. A port that failed to bind is local:
//! taking this instance out of rotation moves traffic to one that bound. A
//! configuration that will not compile is not: every replica reads the same
//! file, so failing readiness everywhere turns a bad edit into an outage,
//! whereas each replica is still serving the last configuration that worked.
//!
//! So local conditions block readiness and shared ones degrade it. Both are
//! reported; only one takes the instance out of the load balancer.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, RwLock};

/// Authoritative configuration failure at the instant a report is sampled.
#[derive(Debug, Clone)]
pub struct ReloadFailure {
    pub attempts: u32,
    pub summary: String,
}

/// Host adapter, called without the Health lock. Implementations must be cheap,
/// perform no I/O, and never recursively call Health::report.
pub trait ReloadStatus: std::fmt::Debug + Send + Sync {
    fn failure(&self) -> Option<ReloadFailure>;
}

/// Atomic log-writer status only; implementations must not do I/O or acquire
/// writer ownership whose destructor joins a logging thread.
pub trait LogStatus: std::fmt::Debug + Send + Sync {
    fn failures(&self) -> Vec<String>;
}

/// Something worth telling an operator about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// No configuration has been accepted yet.
    Starting,
    /// A configuration is live, but nothing is listening — every address in it
    /// failed to bind, or it declared none.
    NoListeners,
    /// Shutting down. In-flight requests are being finished; new ones belong
    /// somewhere else.
    Draining,
    /// Local checkpoint completion is unknown; inspect/restart before more writes.
    PersistenceUnknown,
    /// Serving the last configuration that worked while newer ones are refused.
    ReloadFailing { attempts: u32, summary: String },
    /// Every endpoint of this upstream is out.
    UpstreamDown { upstream: String },
    /// A site asked for an automatic certificate and does not have one yet.
    CertificatePending { site: String },
    /// Logging remains fail-open; expose storage failure without evicting an
    /// otherwise serving gateway from its load balancer.
    LogIoFailed { sink: String },
}

impl Reason {
    /// Fixed, low-cardinality reason labels. Never use diagnostic text or
    /// individual resource identities as labels for aggregate health metrics.
    pub const CODES: [&'static str; 8] = [
        "starting",
        "no_listeners",
        "draining",
        "persistence_unknown",
        "reload_failing",
        "upstream_down",
        "certificate_pending",
        "log_io_failed",
    ];

    /// Whether this instance should be taken out of rotation.
    ///
    /// False for anything every replica would report at the same moment.
    pub fn blocks(&self) -> bool {
        match self {
            Reason::Starting | Reason::NoListeners | Reason::Draining | Reason::PersistenceUnknown => true,
            Reason::ReloadFailing { .. }
            | Reason::UpstreamDown { .. }
            | Reason::CertificatePending { .. }
            | Reason::LogIoFailed { .. } => false,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Reason::Starting => "no configuration has been accepted yet".to_string(),
            Reason::NoListeners => "no listener is bound".to_string(),
            Reason::Draining => "draining".to_string(),
            Reason::PersistenceUnknown => {
                "configuration persistence outcome unknown; inspect before retrying".to_string()
            }
            Reason::ReloadFailing { attempts, summary } => {
                format!("serving the last good configuration after {attempts} failed reload(s): {summary}")
            }
            Reason::UpstreamDown { upstream } => {
                format!("every endpoint of upstream `{upstream}` is unhealthy")
            }
            Reason::CertificatePending { site } => {
                format!("site `{site}` has no certificate yet")
            }
            Reason::LogIoFailed { sink } => format!("log sink `{sink}` has an I/O failure"),
        }
    }

    /// A stable name, for a metric or a machine reading the body.
    pub const fn code(&self) -> &'static str {
        match self {
            Reason::Starting => "starting",
            Reason::NoListeners => "no_listeners",
            Reason::Draining => "draining",
            Reason::PersistenceUnknown => "persistence_unknown",
            Reason::ReloadFailing { .. } => "reload_failing",
            Reason::UpstreamDown { .. } => "upstream_down",
            Reason::CertificatePending { .. } => "certificate_pending",
            Reason::LogIoFailed { .. } => "log_io_failed",
        }
    }
}

/// What a readiness probe should conclude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Serving, with nothing to report.
    Ready,
    /// Serving, but something is wrong that taking this instance out of
    /// rotation would not fix.
    Degraded,
    /// Not serving usefully. Send traffic elsewhere.
    NotReady,
}

impl State {
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Degraded => "degraded",
            State::NotReady => "not ready",
        }
    }

    /// Degraded is still ready: it answers 200 so the instance keeps traffic.
    pub const fn is_serving(self) -> bool {
        !matches!(self, State::NotReady)
    }
}

/// The answer to one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    pub reasons: Vec<Reason>,
}

impl Report {
    /// One line per reason, after a line naming the state.
    pub fn render(&self) -> String {
        let mut out = String::from(self.state.as_str());
        out.push('\n');
        for reason in &self.reasons {
            out.push_str(if reason.blocks() {
                "blocking: "
            } else {
                "degraded: "
            });
            out.push_str(&reason.summary());
            out.push('\n');
        }
        out
    }
}

/// What the process knows about its own condition.
///
/// Written from four places — the transaction, the accept loops, the health
/// supervisor and shutdown — and read by a probe.
#[derive(Debug, Default)]
pub struct Health {
    inner: RwLock<Inner>,
    reload_status: OnceLock<Arc<dyn ReloadStatus>>,
    log_status: OnceLock<Arc<dyn LogStatus>>,
}

#[derive(Debug, Default)]
struct Inner {
    serving: bool,
    listeners: usize,
    draining: bool,
    persistence_unknown: bool,
    failures: u32,
    last_failure: Option<String>,
    down: BTreeSet<String>,
    uncertified: BTreeSet<String>,
}

impl Health {
    pub fn set_log_status(&self, source: Arc<dyn LogStatus>) -> bool {
        self.log_status.set(source).is_ok()
    }
    /// Local durability failure is latched for this host lifetime. A subsequent
    /// in-memory commit cannot establish that disk and memory are reconciled.
    /// Do not fail liveness: automatic restart is not a reconciliation protocol.
    pub fn persistence_unknown(&self) {
        self.write().persistence_unknown = true;
    }

    /// Install once during host construction. When installed, its None means
    /// healthy configuration, not fallback to potentially late push notifications.
    pub fn set_reload_status(&self, source: Arc<dyn ReloadStatus>) -> bool {
        self.reload_status.set(source).is_ok()
    }
    pub fn new() -> Self {
        Self::default()
    }

    /// A generation was published, over this many bound addresses.
    pub fn committed(&self, listeners: usize) {
        let mut inner = self.write();
        inner.serving = true;
        inner.listeners = listeners;
        inner.failures = 0;
        inner.last_failure = None;
    }

    /// A transaction did not produce a generation.
    ///
    /// The count is what separates one bad save from a file that has been
    /// broken for an hour.
    pub fn reload_failed(&self, summary: impl Into<String>) {
        let mut inner = self.write();
        inner.failures = inner.failures.saturating_add(1);
        inner.last_failure = Some(summary.into());
    }

    /// Shutdown has begun. There is no way back from this.
    pub fn draining(&self) {
        self.write().draining = true;
    }

    /// Report an upstream after a probe round.
    pub fn upstream(&self, name: &str, any_healthy: bool) {
        let mut inner = self.write();
        if any_healthy {
            inner.down.remove(name);
        } else {
            inner.down.insert(name.to_string());
        }
    }

    /// Report a site after an issuance attempt.
    pub fn certificate(&self, site: &str, ready: bool) {
        let mut inner = self.write();
        if ready {
            inner.uncertified.remove(site);
        } else {
            inner.uncertified.insert(site.to_string());
        }
    }

    /// Forget sites a new plan no longer names.
    pub fn retain_certificates(&self, live: &BTreeSet<String>) {
        self.write().uncertified.retain(|site| live.contains(site));
    }

    /// Forget upstreams a new plan no longer names.
    pub fn retain_upstreams(&self, live: &BTreeSet<String>) {
        self.write().down.retain(|name| live.contains(name));
    }

    /// Whether a restart would help.
    ///
    /// Nothing here ever says no, and that is deliberate. A liveness probe that
    /// fails on a downstream condition restarts a process that was working,
    /// usually across every replica at once. What it genuinely proves is that
    /// the runtime still schedules tasks and the accept loop still delivers a
    /// request — which is exactly what answering it at all demonstrates.
    pub fn alive(&self) -> bool {
        true
    }

    pub fn report(&self) -> Report {
        let configuration = self.reload_status.get().map(|source| source.failure());
        let log_failures = self
            .log_status
            .get()
            .map(|source| source.failures())
            .unwrap_or_default();
        let inner = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut reasons = Vec::new();
        if inner.persistence_unknown {
            reasons.push(Reason::PersistenceUnknown);
        }
        if inner.draining {
            reasons.push(Reason::Draining);
        } else if !inner.serving {
            reasons.push(Reason::Starting);
        } else if inner.listeners == 0 {
            reasons.push(Reason::NoListeners);
        }
        let failure = configuration.unwrap_or_else(|| {
            (inner.failures > 0).then(|| ReloadFailure {
                attempts: inner.failures,
                summary: inner.last_failure.clone().unwrap_or_default(),
            })
        });
        if let Some(failure) = failure {
            reasons.push(Reason::ReloadFailing {
                attempts: failure.attempts,
                summary: failure.summary,
            });
        }
        for upstream in &inner.down {
            reasons.push(Reason::UpstreamDown {
                upstream: upstream.clone(),
            });
        }
        for site in &inner.uncertified {
            reasons.push(Reason::CertificatePending { site: site.clone() });
        }
        reasons.extend(log_failures.into_iter().map(|sink| Reason::LogIoFailed { sink }));

        let state = if reasons.iter().any(Reason::blocks) {
            State::NotReady
        } else if reasons.is_empty() {
            State::Ready
        } else {
            State::Degraded
        };
        Report { state, reasons }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_failure_blocks_readiness_but_not_liveness_and_survives_commit() {
        let health = Health::new();
        health.committed(1);
        assert_eq!(health.report().state, State::Ready);
        health.persistence_unknown();
        health.persistence_unknown();
        health.committed(1);
        let report = health.report();
        assert_eq!(report.state, State::NotReady);
        assert_eq!(report.reasons, vec![Reason::PersistenceUnknown]);
        assert_eq!(report.reasons[0].code(), "persistence_unknown");
        assert!(
            report
                .render()
                .contains("blocking: configuration persistence outcome unknown")
        );
        assert!(health.alive());
    }

    #[test]
    fn authoritative_reload_status_ignores_late_pushes_and_preserves_other_health() {
        #[derive(Debug)]
        struct Source(RwLock<Option<ReloadFailure>>);
        impl ReloadStatus for Source {
            fn failure(&self) -> Option<ReloadFailure> {
                self.0.read().unwrap().clone()
            }
        }
        let health = Health::new();
        let source = Arc::new(Source(RwLock::new(Some(ReloadFailure {
            attempts: 7,
            summary: "current failure".into(),
        }))));
        assert!(health.set_reload_status(source.clone()));
        assert!(!health.set_reload_status(source.clone()));
        health.committed(1); // Late activation must not erase authoritative failure.
        assert!(health.report().reasons.contains(&Reason::ReloadFailing {
            attempts: 7,
            summary: "current failure".into()
        }));
        *source.0.write().unwrap() = None;
        health.reload_failed("obsolete notification");
        assert_eq!(health.report().state, State::Ready);
        health.certificate("pending", false);
        assert_eq!(
            health.report().reasons,
            vec![Reason::CertificatePending {
                site: "pending".into()
            }]
        );
        health.draining();
        assert_eq!(health.report().state, State::NotReady);
    }

    #[test]
    fn nothing_is_ready_before_a_configuration_is_accepted() {
        let health = Health::new();
        let report = health.report();
        assert_eq!(report.state, State::NotReady);
        assert_eq!(report.reasons, vec![Reason::Starting]);
    }

    #[test]
    fn a_committed_generation_with_a_bound_address_is_ready() {
        let health = Health::new();
        health.committed(1);
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn a_generation_that_bound_nothing_is_not_ready() {
        let health = Health::new();
        health.committed(0);
        let report = health.report();
        assert_eq!(report.state, State::NotReady);
        assert_eq!(report.reasons, vec![Reason::NoListeners]);
    }

    #[test]
    fn a_failing_reload_degrades_rather_than_removing_the_instance() {
        // Every replica reads the same file, so failing readiness here would
        // turn one bad edit into a total outage.
        let health = Health::new();
        health.committed(1);
        health.reload_failed("line 4: unknown directive");
        let report = health.report();
        assert_eq!(report.state, State::Degraded);
        assert!(report.state.is_serving());
        assert_eq!(
            report.reasons,
            vec![Reason::ReloadFailing {
                attempts: 1,
                summary: "line 4: unknown directive".to_string(),
            }]
        );
    }

    #[test]
    fn repeated_failures_are_counted_so_a_stale_one_is_visible() {
        let health = Health::new();
        health.committed(1);
        health.reload_failed("a");
        health.reload_failed("b");
        let Reason::ReloadFailing { attempts, summary } = &health.report().reasons[0] else {
            panic!("expected a failing reload");
        };
        assert_eq!(*attempts, 2);
        assert_eq!(summary, "b", "the most recent reason, not the first");
    }

    #[test]
    fn a_successful_reload_clears_the_failure() {
        let health = Health::new();
        health.committed(1);
        health.reload_failed("broken");
        health.committed(1);
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn an_upstream_that_is_entirely_down_degrades() {
        let health = Health::new();
        health.committed(1);
        health.upstream("api", false);
        let report = health.report();
        assert_eq!(report.state, State::Degraded);
        assert_eq!(
            report.reasons,
            vec![Reason::UpstreamDown {
                upstream: "api".to_string()
            }]
        );
        health.upstream("api", true);
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn an_upstream_a_new_plan_dropped_stops_being_reported() {
        let health = Health::new();
        health.committed(1);
        health.upstream("old", false);
        health.retain_upstreams(&BTreeSet::from(["new".to_string()]));
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn a_site_still_waiting_for_a_certificate_degrades() {
        // Blocking would take every replica out at first boot, and in front of
        // a load balancer that would stop the certificate authority reaching
        // the challenge — so issuance would never finish.
        let health = Health::new();
        health.committed(1);
        health.certificate("example.com", false);
        let report = health.report();
        assert_eq!(report.state, State::Degraded);
        assert!(report.state.is_serving());
        assert_eq!(
            report.reasons,
            vec![Reason::CertificatePending {
                site: "example.com".to_string()
            }]
        );

        health.certificate("example.com", true);
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn a_site_a_new_plan_dropped_stops_being_reported() {
        let health = Health::new();
        health.committed(1);
        health.certificate("old.example.com", false);
        health.retain_certificates(&BTreeSet::from(["new.example.com".to_string()]));
        assert_eq!(health.report().state, State::Ready);
    }

    #[test]
    fn draining_takes_the_instance_out_of_rotation() {
        let health = Health::new();
        health.committed(1);
        health.draining();
        let report = health.report();
        assert_eq!(report.state, State::NotReady);
        assert_eq!(report.reasons, vec![Reason::Draining]);
        assert!(health.alive(), "a draining process is not a wedged one");
    }

    #[test]
    fn a_report_names_every_reason_and_says_which_one_removed_it() {
        let health = Health::new();
        health.committed(0);
        health.reload_failed("bad");
        let rendered = health.report().render();
        assert!(rendered.starts_with("not ready\n"), "{rendered}");
        assert!(rendered.contains("blocking: no listener is bound"), "{rendered}");
        assert!(rendered.contains("degraded: serving the last good"), "{rendered}");
    }

    #[test]
    fn reason_codes_are_stable_and_distinct() {
        let reasons = [
            Reason::Starting,
            Reason::NoListeners,
            Reason::Draining,
            Reason::PersistenceUnknown,
            Reason::ReloadFailing {
                attempts: 1,
                summary: String::new(),
            },
            Reason::UpstreamDown {
                upstream: String::new(),
            },
            Reason::CertificatePending { site: String::new() },
            Reason::LogIoFailed { sink: String::new() },
        ];
        let codes: BTreeSet<_> = reasons.iter().map(|reason| reason.code()).collect();
        assert_eq!(codes.len(), reasons.len());
        assert_eq!(codes, Reason::CODES.into_iter().collect());
    }
}
