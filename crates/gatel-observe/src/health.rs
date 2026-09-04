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
use std::sync::RwLock;

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
    /// Serving the last configuration that worked while newer ones are refused.
    ReloadFailing { attempts: u32, summary: String },
    /// Every endpoint of this upstream is out.
    UpstreamDown { upstream: String },
    /// A site asked for an automatic certificate and does not have one yet.
    CertificatePending { site: String },
}

impl Reason {
    /// Whether this instance should be taken out of rotation.
    ///
    /// False for anything every replica would report at the same moment.
    pub fn blocks(&self) -> bool {
        match self {
            Reason::Starting | Reason::NoListeners | Reason::Draining => true,
            Reason::ReloadFailing { .. }
            | Reason::UpstreamDown { .. }
            | Reason::CertificatePending { .. } => false,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Reason::Starting => "no configuration has been accepted yet".to_string(),
            Reason::NoListeners => "no listener is bound".to_string(),
            Reason::Draining => "draining".to_string(),
            Reason::ReloadFailing { attempts, summary } => {
                format!("serving the last good configuration after {attempts} failed reload(s): {summary}")
            }
            Reason::UpstreamDown { upstream } => {
                format!("every endpoint of upstream `{upstream}` is unhealthy")
            }
            Reason::CertificatePending { site } => {
                format!("site `{site}` has no certificate yet")
            }
        }
    }

    /// A stable name, for a metric or a machine reading the body.
    pub const fn code(&self) -> &'static str {
        match self {
            Reason::Starting => "starting",
            Reason::NoListeners => "no_listeners",
            Reason::Draining => "draining",
            Reason::ReloadFailing { .. } => "reload_failing",
            Reason::UpstreamDown { .. } => "upstream_down",
            Reason::CertificatePending { .. } => "certificate_pending",
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
}

#[derive(Debug, Default)]
struct Inner {
    serving: bool,
    listeners: usize,
    draining: bool,
    failures: u32,
    last_failure: Option<String>,
    down: BTreeSet<String>,
    uncertified: BTreeSet<String>,
}

impl Health {
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
        inner.failures += 1;
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
        let inner = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut reasons = Vec::new();
        if inner.draining {
            reasons.push(Reason::Draining);
        } else if !inner.serving {
            reasons.push(Reason::Starting);
        } else if inner.listeners == 0 {
            reasons.push(Reason::NoListeners);
        }
        if inner.failures > 0 {
            reasons.push(Reason::ReloadFailing {
                attempts: inner.failures,
                summary: inner.last_failure.clone().unwrap_or_default(),
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
            Reason::ReloadFailing {
                attempts: 1,
                summary: String::new(),
            },
            Reason::UpstreamDown {
                upstream: String::new(),
            },
            Reason::CertificatePending { site: String::new() },
        ];
        let codes: BTreeSet<_> = reasons.iter().map(|reason| reason.code()).collect();
        assert_eq!(codes.len(), reasons.len());
    }
}
