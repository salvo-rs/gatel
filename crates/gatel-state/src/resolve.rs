//! Turning a backend's name into addresses, again and again.
//!
//! Resolution is deliberately *not* part of preparing a generation. A name
//! that does not resolve is an unreachable backend, not a broken
//! configuration, and treating it as the second would let a DNS outage take
//! down a gateway whose configuration never changed. So a lookup failing here
//! looks exactly like a health check failing: the endpoint stops being
//! eligible, the event says why, and the next round tries again.
//!
//! The system resolver is used rather than a resolver of our own. That is what
//! makes `/etc/hosts`, `nsswitch`, a container's search domains and a VPN's
//! split DNS mean the same thing for Gatel as for every other process on the
//! machine — which is the whole reason a name was written instead of an
//! address. The cost is that no TTL comes back, so refresh is on the interval
//! the configuration states.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gatel_model::{Fingerprint, Hasher};
use gatel_plan::{Family, Resolve, Target};
use tokio::sync::watch;

use crate::upstream::UpstreamRuntime;

/// Ask the system resolver, and keep only what the family allows.
pub async fn lookup(target: &Target, policy: Resolve) -> Result<Vec<SocketAddr>, String> {
    let Target::Name { host, port } = target else {
        return Ok(target.address().into_iter().collect());
    };
    // `lookup_host` needs an authority, and a name that got this far cannot
    // contain anything that would change how one parses.
    let query = format!("{host}:{port}");
    let found = match tokio::time::timeout(policy.timeout, tokio::net::lookup_host(query)).await {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => return Err(error.to_string()),
        Err(_) => return Err(format!("no answer within {:?}", policy.timeout)),
    };

    narrow(found, policy.family)
}

/// Turn what a resolver said into what the pool will dial.
///
/// Separated from the lookup because this half is ours and can be tested,
/// while the other half is the operating system's and cannot: a machine whose
/// resolver answers every name - which many home and hotel networks do - would
/// otherwise make a test of "this name does not exist" fail for a reason that
/// has nothing to do with the code.
fn narrow(found: impl Iterator<Item = SocketAddr>, family: Family) -> Result<Vec<SocketAddr>, String> {
    let mut addresses: Vec<SocketAddr> = found.filter(|address| family.allows(&address.ip())).collect();
    // A stable order, so "the addresses changed" means the set changed rather
    // than the resolver having rotated its answer, which many do on purpose.
    addresses.sort();
    addresses.dedup();

    if addresses.is_empty() {
        return Err(match family {
            Family::Any => "the name has no addresses".to_string(),
            family => format!("the name has no {} addresses", family.as_str()),
        });
    }
    Ok(addresses)
}

/// What makes a resolver worker the same worker across a reload: the names it
/// looks up and how it looks them up.
pub fn worker_identity(runtime: &UpstreamRuntime) -> Fingerprint {
    let mut hasher = Hasher::new();
    runtime.spec.resolve.hash_into(&mut hasher);
    hasher.update_unordered(&runtime.spec.endpoints, |endpoint, hasher| {
        hasher.update_str(&endpoint.id);
        endpoint.target.hash_into(hasher);
    });
    hasher.finish()
}

/// Keep one upstream's names resolved until asked to stop.
///
/// Runs a round immediately, so a process that has just started does not serve
/// its first requests through the slower on-demand path in the pool.
pub async fn resolve_loop(runtime: Arc<UpstreamRuntime>, stop: watch::Receiver<bool>) {
    let policy = runtime.spec.resolve;
    let upstream = runtime.spec.id.name().to_string();
    run_loop(policy, stop, || round(&runtime, &upstream, policy)).await;
}

async fn run_loop<F, R>(policy: Resolve, mut stop: watch::Receiver<bool>, mut round: F)
where
    F: FnMut() -> R,
    R: std::future::Future<Output = Outcome>,
{
    loop {
        if *stop.borrow() {
            return;
        }
        let outcome = tokio::select! {
            biased;
            _ = stop.changed() => return,
            outcome = async { round().await } => outcome,
        };
        // A name that will not resolve is usually a container that has not
        // finished starting, and the interval a person writes is chosen for
        // steady state. Coming back sooner turns a minute of 503s into a
        // couple of seconds of them.
        let delay = match outcome {
            Outcome::Settled => policy.interval,
            Outcome::SomethingFailed => RETRY_AFTER.min(policy.interval),
        };
        tokio::select! {
            biased;
            _ = stop.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Whether a round left anything unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Settled,
    SomethingFailed,
}

async fn round(runtime: &Arc<UpstreamRuntime>, upstream: &str, policy: Resolve) -> Outcome {
    // Every name at once: twenty backends should cost one round trip's worth
    // of waiting, not twenty.
    let mut lookups = Vec::new();
    for endpoint in runtime.endpoints.all() {
        if !endpoint.target.is_name() {
            continue;
        }
        let endpoint = Arc::clone(endpoint);
        lookups.push(async move {
            let result = lookup(&endpoint.target, policy).await;
            (endpoint, result)
        });
    }
    if lookups.is_empty() {
        return Outcome::Settled;
    }

    let mut outcome = Outcome::Settled;
    for (endpoint, result) in crate::supervisor::join_all(lookups).await {
        match result {
            Ok(addresses) => {
                let Some(change) = endpoint.set_addresses(addresses) else {
                    continue;
                };
                // Connections to an address the name no longer has are
                // connections to a container that has been replaced. In-flight
                // requests finish; nothing new is sent down them.
                let dropped = runtime.pool.retire(&endpoint.id, &change.now);
                tracing::info!(
                    target: "gatel::runtime",
                    event = "endpoint_resolved",
                    upstream,
                    endpoint = endpoint.id.as_str(),
                    addresses = change.describe().as_str(),
                    dropped,
                );
            }
            Err(error) => {
                outcome = Outcome::SomethingFailed;
                if endpoint.record_resolve_failure() {
                    tracing::warn!(
                        target: "gatel::runtime",
                        event = "endpoint_resolve_failed",
                        upstream,
                        endpoint = endpoint.id.as_str(),
                        detail = error.as_str(),
                    );
                }
            }
        }
    }
    outcome
}

/// The addresses an endpoint gained and lost in one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub before: Vec<SocketAddr>,
    pub now: Vec<SocketAddr>,
}

impl Change {
    pub fn describe(&self) -> String {
        self.now
            .iter()
            .map(|address| address.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// How long to wait before the first retry of a name that will not resolve.
///
/// Short, because the usual cause is a container that has not finished
/// starting, and the interval a person writes is chosen for steady state.
pub const RETRY_AFTER: Duration = Duration::from_secs(2);

#[cfg(test)]
#[path = "resolve_cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(family: Family) -> Resolve {
        Resolve {
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            family,
        }
    }

    #[tokio::test]
    async fn an_address_resolves_to_itself_without_asking_anyone() {
        let target = Target::parse("127.0.0.1:8080").unwrap();
        let found = lookup(&target, policy(Family::Any)).await.unwrap();
        assert_eq!(found, vec!["127.0.0.1:8080".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn localhost_resolves_through_the_system_resolver() {
        // The one name every machine agrees about, which is what makes it the
        // only name a test may depend on.
        let target = Target::parse("localhost:8080").unwrap();
        let found = lookup(&target, policy(Family::Any)).await.unwrap();
        assert!(!found.is_empty());
        assert!(found.iter().all(|address| address.ip().is_loopback()));
        assert!(found.iter().all(|address| address.port() == 8080));
    }

    #[tokio::test]
    async fn a_family_narrows_what_comes_back() {
        let target = Target::parse("localhost:8080").unwrap();
        let found = lookup(&target, policy(Family::Ipv4)).await;
        // A machine with no IPv4 loopback would be unusual, but the point of
        // the assertion is the filter, not the machine.
        if let Ok(found) = found {
            assert!(found.iter().all(|address| address.ip().is_ipv4()));
        }
    }

    fn addresses(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|text| text.parse().unwrap()).collect()
    }

    #[test]
    fn an_answer_with_nothing_usable_in_it_is_an_error_rather_than_an_empty_set() {
        // An empty set would leave the endpoint looking resolved and dial
        // nothing, which reads as a hang rather than as a failure.
        let error = narrow(addresses(&[]).into_iter(), Family::Any).unwrap_err();
        assert!(error.contains("no addresses"), "{error}");
    }

    #[test]
    fn a_family_that_excludes_everything_says_which_family_was_asked_for() {
        let found = addresses(&["127.0.0.1:80", "10.0.0.1:80"]);
        let error = narrow(found.into_iter(), Family::Ipv6).unwrap_err();
        assert!(error.contains("no ipv6 addresses"), "{error}");
    }

    #[test]
    fn a_rotated_answer_is_not_a_change() {
        // Resolvers rotate their answers on purpose. If that reached the
        // endpoint, every round would look like a change and drop the pool.
        let one = narrow(
            addresses(&["10.0.0.2:80", "10.0.0.1:80"]).into_iter(),
            Family::Any,
        )
        .unwrap();
        let two = narrow(
            addresses(&["10.0.0.1:80", "10.0.0.2:80"]).into_iter(),
            Family::Any,
        )
        .unwrap();
        assert_eq!(one, two);
    }

    #[test]
    fn one_address_given_twice_is_one_address() {
        // A name with both an A and a CNAME chain to the same host can come
        // back twice; dialling it twice as often would be wrong.
        let found = addresses(&["10.0.0.1:80", "10.0.0.1:80"]);
        assert_eq!(narrow(found.into_iter(), Family::Any).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_answer_comes_back_in_a_stable_order() {
        let target = Target::parse("localhost:8080").unwrap();
        let first = lookup(&target, policy(Family::Any)).await.unwrap();
        let second = lookup(&target, policy(Family::Any)).await.unwrap();
        // Resolvers rotate answers on purpose. If that reached the endpoint it
        // would look like the address set changing every round, and every
        // round would drop the pool.
        assert_eq!(first, second);
    }
}
