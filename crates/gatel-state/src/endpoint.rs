//! The endpoint set and its health.
//!
//! This is mutable state that deliberately sits outside the configuration
//! transaction: a backend going unhealthy is not a configuration change, must
//! not produce a new generation, and must be visible to every generation that
//! shares the upstream.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gatel_plan::{Target, Upstream};

/// What the gateway currently believes about a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// No probe has completed yet. Treated as usable when no active probe is
    /// configured, and as unusable when one is, so a gateway never sends the
    /// first request into a backend it was told to check first.
    Unknown,
    Healthy,
    Unhealthy,
    /// Taken out deliberately. In-flight requests finish; no new ones start.
    Draining,
}

impl Health {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Health::Healthy,
            2 => Health::Unhealthy,
            3 => Health::Draining,
            _ => Health::Unknown,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Health::Unknown => 0,
            Health::Healthy => 1,
            Health::Unhealthy => 2,
            Health::Draining => 3,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Health::Unknown => "unknown",
            Health::Healthy => "healthy",
            Health::Unhealthy => "unhealthy",
            Health::Draining => "draining",
        }
    }
}

/// One backend, with the counters the balancer and health logic read.
///
/// The *target* is the identity and comes from the plan; the *addresses* are
/// what it currently resolves to and come from the network. Keeping health,
/// pool slots and metric labels on the identity is what lets a container be
/// replaced without any of them being reset.
#[derive(Debug)]
pub struct Endpoint {
    pub id: String,
    pub target: Target,
    pub weight: u32,
    pub priority: i32,
    /// Requests currently assigned to this endpoint: from the moment the
    /// balancer picks it until the response body is finished or cancelled,
    /// including time spent queued for a connection. Logical requests, not
    /// sockets, so HTTP/2 multiplexing counts correctly.
    in_flight: AtomicU64,
    health: AtomicU32,
    consecutive_ok: AtomicU32,
    consecutive_fail: AtomicU32,
    /// Passive failures inside the current window.
    passive_fails: AtomicU32,
    window_started: Mutex<Option<Instant>>,
    ejected_until: Mutex<Option<Instant>>,
    /// Smoothed weighted round robin's current credit.
    current_weight: AtomicI64,
    /// What the target currently resolves to. Filled once and never changed
    /// for an address; refreshed on an interval for a name.
    addresses: Mutex<Arc<Vec<SocketAddr>>>,
    /// Where the next dial starts in that list, so a name with three addresses
    /// spreads connections over all three rather than piling them on the first.
    rotation: AtomicU64,
    /// Whether the last lookup failed, so a name that is down reports once
    /// rather than every round.
    failing: AtomicU32,
}

impl Endpoint {
    fn new(spec: &gatel_plan::Endpoint, probed: bool) -> Self {
        Self {
            id: spec.id.clone(),
            target: spec.target.clone(),
            weight: spec.weight,
            priority: spec.priority,
            in_flight: AtomicU64::new(0),
            health: AtomicU32::new(if probed { Health::Unknown } else { Health::Healthy }.as_u8() as u32),
            consecutive_ok: AtomicU32::new(0),
            consecutive_fail: AtomicU32::new(0),
            passive_fails: AtomicU32::new(0),
            window_started: Mutex::new(None),
            ejected_until: Mutex::new(None),
            current_weight: AtomicI64::new(0),
            addresses: Mutex::new(Arc::new(spec.target.address().into_iter().collect())),
            rotation: AtomicU64::new(0),
            failing: AtomicU32::new(0),
        }
    }

    /// What the target resolves to right now. Empty only for a name whose
    /// first lookup has not finished or has never succeeded.
    pub fn addresses(&self) -> Arc<Vec<SocketAddr>> {
        Arc::clone(&self.addresses.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The addresses to try, in the order to try them. Advances the rotation,
    /// so two concurrent dials of a three-address name start in two places.
    pub fn dial_order(&self) -> Vec<SocketAddr> {
        let addresses = self.addresses();
        if addresses.len() < 2 {
            return addresses.as_ref().clone();
        }
        let start = self.rotation.fetch_add(1, Ordering::Relaxed) as usize % addresses.len();
        let mut ordered = Vec::with_capacity(addresses.len());
        ordered.extend_from_slice(&addresses[start..]);
        ordered.extend_from_slice(&addresses[..start]);
        ordered
    }

    /// Record a fresh answer. Returns what changed, or `None` when the answer
    /// is the one already held — which is the common case, and must not cost
    /// the pool anything.
    pub fn set_addresses(&self, addresses: Vec<SocketAddr>) -> Option<crate::resolve::Change> {
        self.failing.store(0, Ordering::Relaxed);
        let mut held = self.addresses.lock().unwrap_or_else(|p| p.into_inner());
        if held.as_slice() == addresses.as_slice() {
            return None;
        }
        let before = held.as_ref().clone();
        *held = Arc::new(addresses.clone());
        Some(crate::resolve::Change {
            before,
            now: addresses,
        })
    }

    /// Note that a lookup failed. Returns whether this is the first failure
    /// since the last success, which is the only one worth an event.
    ///
    /// The addresses already held are kept: a resolver that cannot be reached
    /// is not evidence that the backend moved, and throwing away a working
    /// answer would turn a DNS blip into an outage.
    pub fn record_resolve_failure(&self) -> bool {
        self.failing.fetch_add(1, Ordering::Relaxed) == 0
    }

    /// Whether a lookup has yet to succeed for this endpoint.
    pub fn is_unresolved(&self) -> bool {
        self.addresses().is_empty()
    }

    pub fn health(&self) -> Health {
        Health::from_u8(self.health.load(Ordering::Acquire) as u8)
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn current_weight(&self) -> &AtomicI64 {
        &self.current_weight
    }

    fn set_health(&self, health: Health) {
        self.health.store(health.as_u8() as u32, Ordering::Release);
    }

    fn is_ejected(&self, now: Instant) -> bool {
        let mut ejected = self.ejected_until.lock().unwrap_or_else(|p| p.into_inner());
        match *ejected {
            Some(until) if until > now => true,
            Some(_) => {
                // The cooldown expired. Give it another chance rather than
                // waiting for a probe that may not be configured.
                *ejected = None;
                self.passive_fails.store(0, Ordering::Relaxed);
                false
            }
            None => false,
        }
    }

    /// Eligible to receive a new request right now.
    pub fn is_eligible(&self, now: Instant) -> bool {
        matches!(self.health(), Health::Healthy | Health::Unknown) && !self.is_ejected(now)
    }
}

/// A guard that keeps an endpoint's in-flight count accurate even when a
/// request is cancelled: the count drops when this is dropped, wherever that
/// happens.
pub struct InFlight {
    endpoint: Arc<Endpoint>,
}

impl InFlight {
    pub fn endpoint(&self) -> &Arc<Endpoint> {
        &self.endpoint
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.endpoint.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for InFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlight")
            .field("endpoint", &self.endpoint.id)
            .finish()
    }
}

/// Every backend of one upstream, plus the rules for taking them out.
#[derive(Debug)]
pub struct EndpointSet {
    endpoints: Vec<Arc<Endpoint>>,
    passive: Option<gatel_plan::PassiveHealth>,
    active: Option<gatel_plan::Health>,
}

impl EndpointSet {
    pub fn new(upstream: &Upstream) -> Self {
        let probed = upstream.health.is_some();
        Self {
            endpoints: upstream
                .endpoints
                .iter()
                .map(|spec| Arc::new(Endpoint::new(spec, probed)))
                .collect(),
            passive: upstream.passive_health.clone(),
            active: upstream.health.clone(),
        }
    }

    pub fn all(&self) -> &[Arc<Endpoint>] {
        &self.endpoints
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Endpoint>> {
        self.endpoints.iter().find(|endpoint| endpoint.id == id)
    }

    /// The endpoints a request may be sent to, in a stable order.
    pub fn eligible(&self) -> Vec<Arc<Endpoint>> {
        let now = Instant::now();
        self.endpoints
            .iter()
            .filter(|endpoint| endpoint.is_eligible(now))
            .cloned()
            .collect()
    }

    /// Requests this upstream is handling right now, across every backend.
    ///
    /// The denominator of the retry budget. Counted from the endpoints rather
    /// than kept separately, so it is the same number `least-conn` balances on
    /// and cannot drift away from it.
    pub fn in_flight(&self) -> u64 {
        self.endpoints.iter().map(|endpoint| endpoint.in_flight()).sum()
    }

    /// Claim an endpoint for a request. The returned guard must live until the
    /// response body is finished or dropped.
    pub fn claim(&self, endpoint: &Arc<Endpoint>) -> InFlight {
        endpoint.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight {
            endpoint: Arc::clone(endpoint),
        }
    }

    /// Report the outcome of an active probe.
    pub fn report_probe(&self, id: &str, success: bool) -> Option<Health> {
        let endpoint = self.get(id)?;
        let Some(policy) = &self.active else { return None };
        let before = endpoint.health();
        if success {
            endpoint.consecutive_fail.store(0, Ordering::Relaxed);
            let streak = endpoint.consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
            if streak >= policy.healthy_threshold {
                endpoint.set_health(Health::Healthy);
                // A backend that passes its probe has recovered, whatever live
                // traffic said a moment ago.
                *endpoint.ejected_until.lock().unwrap_or_else(|p| p.into_inner()) = None;
                endpoint.passive_fails.store(0, Ordering::Relaxed);
            }
        } else {
            endpoint.consecutive_ok.store(0, Ordering::Relaxed);
            let streak = endpoint.consecutive_fail.fetch_add(1, Ordering::Relaxed) + 1;
            if streak >= policy.unhealthy_threshold {
                endpoint.set_health(Health::Unhealthy);
            }
        }
        let after = endpoint.health();
        (before != after).then_some(after)
    }

    /// Report what live traffic saw. Returns `true` if the endpoint was just
    /// ejected.
    pub fn report_traffic(&self, id: &str, failed: bool) -> bool {
        let Some(policy) = &self.passive else { return false };
        let Some(endpoint) = self.get(id) else {
            return false;
        };
        let now = Instant::now();

        if !failed {
            // A success does not clear the window on its own; only time does.
            // Otherwise one lucky request hides a backend failing half of them.
            return false;
        }

        let mut window = endpoint.window_started.lock().unwrap_or_else(|p| p.into_inner());
        match *window {
            Some(started) if now.duration_since(started) < policy.window => {}
            _ => {
                *window = Some(now);
                endpoint.passive_fails.store(0, Ordering::Relaxed);
            }
        }
        drop(window);

        let fails = endpoint.passive_fails.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= policy.max_fails {
            let mut ejected = endpoint.ejected_until.lock().unwrap_or_else(|p| p.into_inner());
            if ejected.is_none() {
                *ejected = Some(now + policy.cooldown);
                return true;
            }
        }
        false
    }

    /// Whether an upstream response status counts as a failure.
    pub fn counts_as_failure(&self, status: u16) -> bool {
        self.passive
            .as_ref()
            .is_some_and(|policy| policy.unhealthy_status.contains(&status))
    }

    pub fn has_passive_health(&self) -> bool {
        self.passive.is_some()
    }

    pub fn active_policy(&self) -> Option<&gatel_plan::Health> {
        self.active.as_ref()
    }

    /// Take an endpoint out of rotation without changing configuration.
    pub fn drain(&self, id: &str) -> bool {
        match self.get(id) {
            Some(endpoint) => {
                endpoint.set_health(Health::Draining);
                true
            }
            None => false,
        }
    }

    pub fn undrain(&self, id: &str) -> bool {
        match self.get(id) {
            Some(endpoint) if endpoint.health() == Health::Draining => {
                endpoint.set_health(if self.active.is_some() {
                    Health::Unknown
                } else {
                    Health::Healthy
                });
                endpoint.consecutive_ok.store(0, Ordering::Relaxed);
                endpoint.consecutive_fail.store(0, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

/// How long until the next probe of an endpoint, spread so a hundred backends
/// are not all probed in the same millisecond.
pub fn probe_delay(policy: &gatel_plan::Health, index: usize, tick: u64) -> Duration {
    if policy.jitter.is_zero() {
        return policy.interval;
    }
    let spread = policy.jitter.as_millis() as u64;
    let offset = (index as u64).wrapping_mul(2_654_435_761).wrapping_add(tick) % (spread * 2 + 1);
    policy.interval + Duration::from_millis(offset) - policy.jitter
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, ResourceId, ResourceKind, SourceId};

    use super::*;

    fn upstream(active: bool, passive: Option<gatel_plan::PassiveHealth>) -> Upstream {
        Upstream {
            resolve: gatel_plan::Resolve::default(),
            id: ResourceId::new(ResourceKind::Upstream, "api"),
            owner: SourceId::file("main"),
            endpoints: ["127.0.0.1:1", "127.0.0.1:2"]
                .iter()
                .map(|address| gatel_plan::Endpoint {
                    id: (*address).to_string(),
                    target: gatel_plan::Target::parse(address).unwrap(),
                    weight: 1,
                    priority: 0,
                    origin: Origin::synthetic("test"),
                })
                .collect(),
            balance: Default::default(),
            timeouts: Default::default(),
            pool: Default::default(),
            retry: None,
            health: active.then(|| gatel_plan::Health {
                kind: gatel_plan::HealthKind::TcpConnect,
                interval: Duration::from_secs(1),
                timeout: Duration::from_millis(500),
                healthy_threshold: 2,
                unhealthy_threshold: 2,
                jitter: Duration::ZERO,
            }),
            passive_health: passive,
            protocol: Default::default(),
            tls: None,
            origin: Origin::synthetic("test"),
        }
    }

    fn passive(max_fails: u32, cooldown: Duration) -> gatel_plan::PassiveHealth {
        gatel_plan::PassiveHealth {
            max_fails,
            window: Duration::from_secs(30),
            cooldown,
            unhealthy_status: vec![502, 503],
        }
    }

    #[test]
    fn without_an_active_probe_endpoints_start_usable() {
        let set = EndpointSet::new(&upstream(false, None));
        assert_eq!(set.eligible().len(), 2);
        assert_eq!(set.all()[0].health(), Health::Healthy);
    }

    #[test]
    fn with_an_active_probe_endpoints_wait_to_be_checked() {
        let set = EndpointSet::new(&upstream(true, None));
        assert_eq!(set.all()[0].health(), Health::Unknown);
        // Unknown is still usable: refusing all traffic until the first probe
        // would make every start a brief outage.
        assert_eq!(set.eligible().len(), 2);
    }

    #[test]
    fn probes_need_consecutive_results_to_change_a_verdict() {
        let set = EndpointSet::new(&upstream(true, None));
        assert_eq!(
            set.report_probe("127.0.0.1:1", false),
            None,
            "one failure is not enough"
        );
        assert_eq!(set.report_probe("127.0.0.1:1", false), Some(Health::Unhealthy));
        assert_eq!(set.eligible().len(), 1);

        assert_eq!(set.report_probe("127.0.0.1:1", true), None);
        assert_eq!(set.report_probe("127.0.0.1:1", true), Some(Health::Healthy));
        assert_eq!(set.eligible().len(), 2);
    }

    #[test]
    fn a_single_success_resets_a_failure_streak() {
        let set = EndpointSet::new(&upstream(true, None));
        set.report_probe("127.0.0.1:1", false);
        set.report_probe("127.0.0.1:1", true);
        assert_eq!(
            set.report_probe("127.0.0.1:1", false),
            None,
            "the streak restarted"
        );
    }

    #[test]
    fn passive_health_ejects_after_enough_failures_and_recovers_after_the_cooldown() {
        let set = EndpointSet::new(&upstream(false, Some(passive(2, Duration::from_millis(30)))));
        assert!(!set.report_traffic("127.0.0.1:1", true));
        assert!(
            set.report_traffic("127.0.0.1:1", true),
            "the second failure ejects"
        );
        assert_eq!(set.eligible().len(), 1);

        std::thread::sleep(Duration::from_millis(45));
        assert_eq!(set.eligible().len(), 2, "the cooldown expired");
    }

    #[test]
    fn passive_health_is_off_unless_configured() {
        let set = EndpointSet::new(&upstream(false, None));
        for _ in 0..10 {
            assert!(!set.report_traffic("127.0.0.1:1", true));
        }
        assert_eq!(set.eligible().len(), 2);
    }

    #[test]
    fn only_configured_statuses_count_as_failures() {
        let set = EndpointSet::new(&upstream(false, Some(passive(1, Duration::from_secs(1)))));
        assert!(set.counts_as_failure(503));
        assert!(!set.counts_as_failure(404));
        assert!(!set.counts_as_failure(500));
    }

    #[test]
    fn a_passing_probe_cancels_a_passive_ejection() {
        let set = EndpointSet::new(&upstream(true, Some(passive(1, Duration::from_secs(60)))));
        assert!(set.report_traffic("127.0.0.1:1", true));
        assert_eq!(set.eligible().len(), 1);

        set.report_probe("127.0.0.1:1", true);
        set.report_probe("127.0.0.1:1", true);
        assert_eq!(
            set.eligible().len(),
            2,
            "the probe is authoritative over a stale ejection"
        );
    }

    #[test]
    fn draining_removes_an_endpoint_without_touching_configuration() {
        let set = EndpointSet::new(&upstream(false, None));
        assert!(set.drain("127.0.0.1:1"));
        assert_eq!(set.eligible().len(), 1);
        assert!(set.undrain("127.0.0.1:1"));
        assert_eq!(set.eligible().len(), 2);
        assert!(!set.drain("127.0.0.1:9"), "an unknown endpoint cannot be drained");
    }

    #[test]
    fn in_flight_counts_fall_even_when_a_request_is_dropped() {
        let set = EndpointSet::new(&upstream(false, None));
        let endpoint = Arc::clone(&set.all()[0]);
        {
            let _first = set.claim(&endpoint);
            let _second = set.claim(&endpoint);
            assert_eq!(endpoint.in_flight(), 2);
        }
        assert_eq!(
            endpoint.in_flight(),
            0,
            "guards must release on drop, not on success"
        );
    }

    #[test]
    fn probe_delays_are_spread_but_stay_near_the_interval() {
        let policy = gatel_plan::Health {
            kind: gatel_plan::HealthKind::TcpConnect,
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(1),
            healthy_threshold: 1,
            unhealthy_threshold: 1,
            jitter: Duration::from_secs(1),
        };
        let delays: Vec<Duration> = (0..8).map(|index| probe_delay(&policy, index, 0)).collect();
        assert!(delays.iter().all(|delay| *delay >= Duration::from_secs(9)));
        assert!(delays.iter().all(|delay| *delay <= Duration::from_secs(11)));
        assert!(
            delays.windows(2).any(|pair| pair[0] != pair[1]),
            "delays must actually differ"
        );
    }
}
