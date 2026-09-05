//! Choosing among eligible endpoints.
//!
//! Every strategy is a pure function of the eligible set, the selector's own
//! state and a request key, so each one is testable without a socket. Only
//! endpoints the health gate already approved are ever passed in: a balancer
//! cannot resurrect a backend the gateway decided not to use.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gatel_plan::Balance;

use crate::endpoint::Endpoint;

/// What the balancer is allowed to know about the request.
#[derive(Debug, Default, Clone)]
pub struct RequestKey<'a> {
    pub client_ip: Option<std::net::IpAddr>,
    /// Origin-form path and query, unmodified: no percent-decoding, so two
    /// spellings of the same path are two keys, exactly as the client sent them.
    pub uri: &'a str,
    pub header: Option<&'a str>,
    pub cookie: Option<&'a str>,
}

/// Counters a strategy keeps between requests. Shared across generations when
/// the reload was compatible, so a rolling reload does not restart the cursor.
#[derive(Debug, Default)]
pub struct SelectorState {
    cursor: AtomicU64,
    random: AtomicU64,
}

impl SelectorState {
    pub fn new() -> Self {
        Self {
            cursor: AtomicU64::new(0),
            random: AtomicU64::new(seed()),
        }
    }

    fn next_random(&self) -> u64 {
        // xorshift64*: no dependency, uniform enough to spread load, and
        // deliberately not used for anything security-sensitive.
        let mut value = self.random.load(Ordering::Relaxed);
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.random.store(value, Ordering::Relaxed);
        value.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    nanos | 1
}

/// Which endpoint this request goes to, and why.
#[derive(Debug, Clone)]
pub struct Selection {
    pub endpoint: Arc<Endpoint>,
    /// The strategy that actually decided, which differs from the configured
    /// one when a hash key was missing.
    pub effective: &'static str,
    pub fell_back: bool,
}

/// Pick an endpoint, or `None` when the eligible set is empty.
pub fn select(
    balance: &Balance,
    eligible: &[Arc<Endpoint>],
    state: &SelectorState,
    key: &RequestKey<'_>,
) -> Option<Selection> {
    if eligible.is_empty() {
        return None;
    }
    if eligible.len() == 1 {
        return Some(Selection {
            endpoint: Arc::clone(&eligible[0]),
            effective: name(balance),
            fell_back: false,
        });
    }

    let chosen = match balance {
        Balance::RoundRobin => round_robin(eligible, state),
        Balance::WeightedRoundRobin => weighted_round_robin(eligible),
        Balance::Random => Arc::clone(&eligible[(state.next_random() % eligible.len() as u64) as usize]),
        Balance::TwoRandomChoices => two_random_choices(eligible, state),
        Balance::LeastConn => least_conn(eligible),
        Balance::First => first(eligible),
        Balance::IpHash => match key.client_ip {
            Some(ip) => rendezvous(eligible, &ip.to_string()),
            None => return fallback(eligible, state),
        },
        Balance::UriHash => rendezvous(eligible, key.uri),
        Balance::HeaderHash { .. } => match key.header {
            Some(value) => rendezvous(eligible, value),
            None => return fallback(eligible, state),
        },
        Balance::CookieHash { .. } => match key.cookie {
            Some(value) => rendezvous(eligible, value),
            None => return fallback(eligible, state),
        },
    };
    Some(Selection {
        endpoint: chosen,
        effective: name(balance),
        fell_back: false,
    })
}

/// A hash strategy with no key to hash falls back to round robin rather than
/// pinning every anonymous request to one backend.
fn fallback(eligible: &[Arc<Endpoint>], state: &SelectorState) -> Option<Selection> {
    Some(Selection {
        endpoint: round_robin(eligible, state),
        effective: "round-robin",
        fell_back: true,
    })
}

fn name(balance: &Balance) -> &'static str {
    match balance {
        Balance::RoundRobin => "round-robin",
        Balance::WeightedRoundRobin => "weighted-round-robin",
        Balance::Random => "random",
        Balance::TwoRandomChoices => "two-random-choices",
        Balance::LeastConn => "least-conn",
        Balance::IpHash => "ip-hash",
        Balance::UriHash => "uri-hash",
        Balance::HeaderHash { .. } => "header-hash",
        Balance::CookieHash { .. } => "cookie-hash",
        Balance::First => "first",
    }
}

fn round_robin(eligible: &[Arc<Endpoint>], state: &SelectorState) -> Arc<Endpoint> {
    let index = state.cursor.fetch_add(1, Ordering::Relaxed) % eligible.len() as u64;
    Arc::clone(&eligible[index as usize])
}

/// Smooth weighted round robin, as nginx does it: weights are distributed
/// evenly through the sequence rather than in runs, so `3:1` alternates
/// `a b a a` rather than sending three requests to `a` and then one to `b`.
fn weighted_round_robin(eligible: &[Arc<Endpoint>]) -> Arc<Endpoint> {
    let total: i64 = eligible.iter().map(|endpoint| endpoint.weight as i64).sum();
    let mut best: Option<(&Arc<Endpoint>, i64)> = None;
    for endpoint in eligible {
        let credit = endpoint
            .current_weight()
            .fetch_add(endpoint.weight as i64, Ordering::Relaxed)
            + endpoint.weight as i64;
        if best.is_none_or(|(_, highest)| credit > highest) {
            best = Some((endpoint, credit));
        }
    }
    let (winner, _) = best.expect("the eligible set is not empty");
    winner.current_weight().fetch_sub(total, Ordering::Relaxed);
    Arc::clone(winner)
}

/// Pick two at random and take the less busy one. Avoids the stampede that
/// plain least-connections causes when a fresh backend appears.
fn two_random_choices(eligible: &[Arc<Endpoint>], state: &SelectorState) -> Arc<Endpoint> {
    let length = eligible.len() as u64;
    let first = (state.next_random() % length) as usize;
    let mut second = (state.next_random() % length) as usize;
    if second == first {
        second = (second + 1) % eligible.len();
    }
    let a = &eligible[first];
    let b = &eligible[second];
    Arc::clone(if a.in_flight() <= b.in_flight() { a } else { b })
}

fn least_conn(eligible: &[Arc<Endpoint>]) -> Arc<Endpoint> {
    let mut best = &eligible[0];
    for endpoint in &eligible[1..] {
        // Ties break on identity, never on position, so the same set always
        // makes the same choice.
        if (endpoint.in_flight(), endpoint.id.as_str()) < (best.in_flight(), best.id.as_str()) {
            best = endpoint;
        }
    }
    Arc::clone(best)
}

fn first(eligible: &[Arc<Endpoint>]) -> Arc<Endpoint> {
    let mut best = &eligible[0];
    for endpoint in &eligible[1..] {
        if (endpoint.priority, endpoint.id.as_str()) < (best.priority, best.id.as_str()) {
            best = endpoint;
        }
    }
    Arc::clone(best)
}

/// Rendezvous hashing: score every endpoint for this key and take the highest.
///
/// Chosen over modulo hashing because removing one backend only moves the keys
/// that pointed at it, instead of remapping nearly everything.
fn rendezvous(eligible: &[Arc<Endpoint>], key: &str) -> Arc<Endpoint> {
    let mut best = &eligible[0];
    let mut best_score = score(key, &eligible[0].id);
    for endpoint in &eligible[1..] {
        let candidate = score(key, &endpoint.id);
        if (candidate, endpoint.id.as_str()) > (best_score, best.id.as_str()) {
            best = endpoint;
            best_score = candidate;
        }
    }
    Arc::clone(best)
}

fn score(key: &str, endpoint_id: &str) -> u64 {
    // FNV-1a over both parts with a separator, so ("ab","c") and ("a","bc")
    // cannot collide.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes().chain(std::iter::once(0)).chain(endpoint_id.bytes()) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, ResourceId, ResourceKind, SourceId};
    use gatel_plan::Upstream;

    use super::*;
    use crate::endpoint::EndpointSet;

    fn set(specs: &[(&str, u32, i32)]) -> EndpointSet {
        let upstream = Upstream {
            id: ResourceId::new(ResourceKind::Upstream, "api"),
            owner: SourceId::file("main"),
            endpoints: specs
                .iter()
                .enumerate()
                .map(|(index, (id, weight, priority))| gatel_plan::Endpoint {
                    id: (*id).to_string(),
                    target: gatel_plan::Target::parse(&format!("127.0.0.1:{}", 9000 + index)).unwrap(),
                    weight: *weight,
                    priority: *priority,
                    origin: Origin::synthetic("test"),
                })
                .collect(),
            balance: Default::default(),
            timeouts: Default::default(),
            pool: Default::default(),
            retry: None,
            health: None,
            passive_health: None,
            protocol: Default::default(),
            tls: None,
            resolve: Default::default(),
            origin: Origin::synthetic("test"),
        };
        EndpointSet::new(&upstream)
    }

    fn plain() -> EndpointSet {
        set(&[("a", 1, 0), ("b", 1, 0), ("c", 1, 0)])
    }

    fn pick(balance: &Balance, set: &EndpointSet, state: &SelectorState, key: &RequestKey<'_>) -> String {
        select(balance, &set.eligible(), state, key)
            .unwrap()
            .endpoint
            .id
            .clone()
    }

    #[test]
    fn round_robin_visits_every_endpoint_in_turn() {
        let set = plain();
        let state = SelectorState::new();
        let key = RequestKey::default();
        let picks: Vec<String> = (0..6)
            .map(|_| pick(&Balance::RoundRobin, &set, &state, &key))
            .collect();
        assert_eq!(picks, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn weighted_round_robin_spreads_rather_than_batches() {
        let set = set(&[("a", 3, 0), ("b", 1, 0)]);
        let state = SelectorState::new();
        let key = RequestKey::default();
        let picks: Vec<String> = (0..8)
            .map(|_| pick(&Balance::WeightedRoundRobin, &set, &state, &key))
            .collect();
        assert_eq!(picks.iter().filter(|id| *id == "a").count(), 6);
        assert_eq!(picks.iter().filter(|id| *id == "b").count(), 2);
        // The heavy endpoint must not take three in a row before the light one.
        assert_ne!(picks[..4], ["a", "a", "a", "b"]);
    }

    #[test]
    fn least_conn_prefers_the_idle_endpoint() {
        let set = plain();
        let state = SelectorState::new();
        let key = RequestKey::default();
        let busy = Arc::clone(set.get("a").unwrap());
        let _guards: Vec<_> = (0..5).map(|_| set.claim(&busy)).collect();
        assert_ne!(pick(&Balance::LeastConn, &set, &state, &key), "a");
    }

    #[test]
    fn least_conn_breaks_ties_by_identity_not_position() {
        let forward = set(&[("a", 1, 0), ("b", 1, 0)]);
        let reversed = set(&[("b", 1, 0), ("a", 1, 0)]);
        let state = SelectorState::new();
        let key = RequestKey::default();
        assert_eq!(
            pick(&Balance::LeastConn, &forward, &state, &key),
            pick(&Balance::LeastConn, &reversed, &state, &key)
        );
    }

    #[test]
    fn first_follows_priority_and_falls_through_when_one_is_removed() {
        let set = set(&[("standby", 1, 10), ("primary", 1, 0)]);
        let state = SelectorState::new();
        let key = RequestKey::default();
        assert_eq!(pick(&Balance::First, &set, &state, &key), "primary");
        set.drain("primary");
        assert_eq!(pick(&Balance::First, &set, &state, &key), "standby");
    }

    #[test]
    fn hashing_sends_the_same_key_to_the_same_endpoint() {
        let set = plain();
        let state = SelectorState::new();
        let key = RequestKey {
            client_ip: Some("203.0.113.7".parse().unwrap()),
            ..Default::default()
        };
        let first = pick(&Balance::IpHash, &set, &state, &key);
        for _ in 0..20 {
            assert_eq!(pick(&Balance::IpHash, &set, &state, &key), first);
        }
    }

    #[test]
    fn rendezvous_hashing_barely_moves_keys_when_an_endpoint_leaves() {
        let full = set(&[("a", 1, 0), ("b", 1, 0), ("c", 1, 0), ("d", 1, 0)]);
        let state = SelectorState::new();
        let keys: Vec<String> = (0..400).map(|index| format!("/page/{index}")).collect();

        let before: Vec<String> = keys
            .iter()
            .map(|uri| {
                pick(
                    &Balance::UriHash,
                    &full,
                    &state,
                    &RequestKey {
                        uri,
                        ..Default::default()
                    },
                )
            })
            .collect();

        full.drain("c");
        let after: Vec<String> = keys
            .iter()
            .map(|uri| {
                pick(
                    &Balance::UriHash,
                    &full,
                    &state,
                    &RequestKey {
                        uri,
                        ..Default::default()
                    },
                )
            })
            .collect();

        let moved = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        let previously_on_c = before.iter().filter(|id| *id == "c").count();
        assert_eq!(
            moved, previously_on_c,
            "only the removed endpoint's keys may move"
        );
        assert!(
            moved < keys.len() / 2,
            "modulo hashing would have moved most keys"
        );
    }

    #[test]
    fn a_missing_hash_key_falls_back_to_round_robin_and_says_so() {
        let set = plain();
        let state = SelectorState::new();
        let key = RequestKey::default();
        let selection = select(
            &Balance::HeaderHash {
                header: "x-tenant".into(),
            },
            &set.eligible(),
            &state,
            &key,
        )
        .unwrap();
        assert!(selection.fell_back);
        assert_eq!(selection.effective, "round-robin");
    }

    #[test]
    fn no_eligible_endpoints_means_no_selection() {
        let set = plain();
        for id in ["a", "b", "c"] {
            set.drain(id);
        }
        assert!(
            select(
                &Balance::RoundRobin,
                &set.eligible(),
                &SelectorState::new(),
                &RequestKey::default()
            )
            .is_none()
        );
    }

    #[test]
    fn random_reaches_every_endpoint() {
        let set = plain();
        let state = SelectorState::new();
        let key = RequestKey::default();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            seen.insert(pick(&Balance::Random, &set, &state, &key));
        }
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn two_random_choices_avoids_the_busiest_endpoint() {
        let set = set(&[("a", 1, 0), ("b", 1, 0)]);
        let state = SelectorState::new();
        let key = RequestKey::default();
        let busy = Arc::clone(set.get("a").unwrap());
        let _guards: Vec<_> = (0..50).map(|_| set.claim(&busy)).collect();
        let picks: Vec<String> = (0..30)
            .map(|_| pick(&Balance::TwoRandomChoices, &set, &state, &key))
            .collect();
        assert!(picks.iter().all(|id| id == "b"));
    }
}
