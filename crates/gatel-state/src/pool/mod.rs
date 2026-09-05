//! The upstream connection pool.
//!
//! Bounded on purpose, and bounded by configuration rather than by a constant:
//! a gateway in front of two backends and one in front of two hundred want
//! very different numbers. Capacity is a semaphore, so waiting for a slot is
//! ordinary backpressure rather than an unbounded queue, and the wait is
//! covered by the `queue` timeout.
//!
//! Three files, along the seams a connection actually has:
//!
//! - here, the pool itself: what it holds per endpoint, how a reload changes its numbers without touching its
//!   sockets, and how one is checked out;
//! - [`dial`], everything involved in making a connection that does not exist yet — resolving, rotating, TLS,
//!   and the HTTP handshake;
//! - [`lease`], one borrowed connection and what happens when it is let go, which is the only place that
//!   decides whether a socket is parked or closed.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gatel_plan::{Timeouts, UpstreamProtocol};
use http_body_util::combinators::BoxBody;
use tokio::sync::Semaphore;

use crate::endpoint::Endpoint;

mod dial;
mod lease;

pub use dial::Sender;
pub use lease::Lease;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxError>;

/// Why a connection could not be obtained.
///
/// Every variant here happened before any request byte reached the upstream,
/// which is what makes retrying on another endpoint safe for any method.
#[derive(Debug, Clone)]
pub enum CheckoutError {
    /// More requests are already waiting than the pool allows.
    QueueFull { waiting: u32, limit: u32 },
    /// Waited longer than `timeouts.queue` for a slot.
    QueueTimeout(Duration),
    /// Waited longer than `timeouts.connect` to reach the backend.
    ConnectTimeout(Duration),
    /// The connection attempt itself failed.
    Connect(String),
    /// The backend is named, and the name could not be turned into an address.
    Resolve(String),
    /// The connection opened but the HTTP handshake failed.
    Handshake(String),
    /// The pool is shutting down.
    Draining,
}

impl CheckoutError {
    /// Whether another endpoint may be tried. Always true here, by
    /// construction: nothing in this module can fail after a request was sent.
    pub const fn is_pre_send(&self) -> bool {
        true
    }

    pub fn summary(&self) -> String {
        match self {
            CheckoutError::QueueFull { waiting, limit } => {
                format!("{waiting} requests already queued, limit is {limit}")
            }
            CheckoutError::QueueTimeout(after) => format!("no connection slot within {after:?}"),
            CheckoutError::ConnectTimeout(after) => format!("could not connect within {after:?}"),
            CheckoutError::Connect(message) => format!("connect failed: {message}"),
            CheckoutError::Resolve(message) => format!("could not resolve the backend: {message}"),
            CheckoutError::Handshake(message) => format!("handshake failed: {message}"),
            CheckoutError::Draining => "the pool is draining".to_string(),
        }
    }
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for CheckoutError {}

pub(super) struct Idle {
    pub(super) sender: Sender,
    /// Which address this connection actually reached, for a target that has
    /// one. Kept so that when a name stops resolving to an address, the
    /// sockets to it can be dropped and the ones to addresses it still has can
    /// be left alone.
    ///
    /// `None` for a Unix socket, which has no address and cannot go stale that
    /// way: the path either opens or it does not.
    pub(super) address: Option<SocketAddr>,
    pub(super) opened: Instant,
    pub(super) returned: Instant,
}

impl Idle {
    pub(super) fn is_usable(&self, config: &gatel_plan::Pool, now: Instant) -> bool {
        if now.duration_since(self.opened) >= config.max_lifetime {
            return false;
        }
        if now.duration_since(self.returned) >= config.idle_timeout {
            return false;
        }
        match &self.sender {
            Sender::Http1(sender) => !sender.is_closed(),
            Sender::Http2(sender) => !sender.is_closed(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Counters {
    pub(super) opened: AtomicU64,
    pub(super) reused: AtomicU64,
    pub(super) discarded: AtomicU64,
}

pub(super) struct EndpointPool {
    pub(super) permits: Arc<Semaphore>,
    /// How many permits the semaphore has been given. The semaphore itself
    /// only reports what is *available*, which is not the same number while
    /// requests are in flight, so the total has to be remembered to resize it.
    granted: AtomicU32,
    pub(super) idle: Mutex<VecDeque<Idle>>,
    pub(super) waiting: AtomicU64,
    pub(super) counters: Counters,
    /// The addresses this endpoint currently has, or empty for "everything is
    /// still current". Read when a connection is returned, so one that reached
    /// a replaced container is dropped rather than parked.
    pub(super) live: Mutex<Vec<SocketAddr>>,
}

/// Live connections to one upstream's backends.
pub struct ConnectionPool {
    /// Behind a lock because a pool outlives the generation that created it.
    /// Sizing is deliberately not part of what makes a pool reusable — raising
    /// a limit must not tear down live connections — which means the pool that
    /// a reload keeps is holding the *previous* generation's numbers until it
    /// is told the new ones.
    policy: Mutex<Policy>,
    protocol: UpstreamProtocol,
    /// Present exactly when the configuration said the backend speaks TLS.
    /// Built during prepare, so a certificate authority that will not load
    /// stops a reload rather than turning into a connection error under
    /// traffic, where it would look like the backend's fault.
    tls: Option<Arc<gatel_tls::ClientTls>>,
    endpoints: Mutex<HashMap<String, Arc<EndpointPool>>>,
}

impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("protocol", &self.protocol)
            .field("max_per_endpoint", &self.policy().config.max_per_endpoint)
            .field("tls", &self.tls.as_ref().map(|tls| tls.server_name()))
            .finish()
    }
}

/// Everything about a pool that a reload may change while keeping the pool.
#[derive(Debug, Clone, Copy)]
struct Policy {
    config: gatel_plan::Pool,
    /// Only used on the on-demand path, when a request arrives before the
    /// resolver task has answered for the first time.
    resolve: gatel_plan::Resolve,
}

/// Grow or shrink one endpoint's concurrency bound in place.
///
/// Growing is immediate. Shrinking can only give back permits nobody is
/// holding, so a limit lowered while requests are in flight lands as far as it
/// can now and the rest at the next commit — which is better than the
/// alternatives: cancelling a request that is already talking to a backend, or
/// waiting on a lock in the middle of a reload.
fn resize(slot: &EndpointPool, wanted: u32) {
    let granted = slot.granted.load(Ordering::Acquire);
    match wanted.cmp(&granted) {
        std::cmp::Ordering::Equal => {}
        std::cmp::Ordering::Greater => {
            slot.permits.add_permits((wanted - granted) as usize);
            slot.granted.store(wanted, Ordering::Release);
        }
        std::cmp::Ordering::Less => {
            let forgotten = slot.permits.forget_permits((granted - wanted) as usize);
            slot.granted.fetch_sub(forgotten as u32, Ordering::AcqRel);
        }
    }
}

/// A snapshot of one endpoint's pool, for metrics and `explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    pub endpoint: String,
    pub idle: usize,
    pub in_use: u32,
    pub waiting: u64,
    pub opened: u64,
    pub reused: u64,
    pub discarded: u64,
}

impl ConnectionPool {
    pub fn new(
        config: gatel_plan::Pool,
        protocol: UpstreamProtocol,
        resolve: gatel_plan::Resolve,
        tls: Option<Arc<gatel_tls::ClientTls>>,
    ) -> Self {
        Self {
            policy: Mutex::new(Policy { config, resolve }),
            protocol,
            tls,
            endpoints: Mutex::new(HashMap::new()),
        }
    }

    fn policy(&self) -> Policy {
        *self.policy.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Take on a reload's numbers without touching its connections.
    ///
    /// Called on every commit, including one that created this pool, so there
    /// is one path rather than two. Without it, everything a reload can change
    /// about a pool that it also keeps — every limit, every lifetime, how often
    /// a name is looked up — would be read once at creation and then silently
    /// ignored for as long as the process ran.
    pub fn adopt(&self, config: gatel_plan::Pool, resolve: gatel_plan::Resolve) {
        *self.policy.lock().unwrap_or_else(|p| p.into_inner()) = Policy { config, resolve };

        let endpoints = self.endpoints.lock().unwrap_or_else(|p| p.into_inner());
        for slot in endpoints.values() {
            resize(slot, config.max_per_endpoint);
        }
    }

    fn slot(&self, endpoint: &Endpoint, config: &gatel_plan::Pool) -> Arc<EndpointPool> {
        let mut endpoints = self.endpoints.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(endpoints.entry(endpoint.id.clone()).or_insert_with(|| {
            Arc::new(EndpointPool {
                permits: Arc::new(Semaphore::new(config.max_per_endpoint as usize)),
                granted: AtomicU32::new(config.max_per_endpoint),
                idle: Mutex::new(VecDeque::new()),
                waiting: AtomicU64::new(0),
                counters: Counters::default(),
                live: Mutex::new(Vec::new()),
            })
        }))
    }

    /// Obtain a connection to `endpoint`, reusing an idle one when possible.
    pub async fn checkout(
        &self,
        endpoint: &Arc<Endpoint>,
        timeouts: &Timeouts,
    ) -> Result<Lease, CheckoutError> {
        let Policy { config, .. } = self.policy();
        let slot = self.slot(endpoint, &config);

        let waiting = slot.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        let _waiting_guard = WaitingGuard {
            slot: Arc::clone(&slot),
        };
        if waiting > config.max_waiters as u64 {
            return Err(CheckoutError::QueueFull {
                waiting: waiting as u32,
                limit: config.max_waiters,
            });
        }

        let permit =
            match tokio::time::timeout(timeouts.queue, Arc::clone(&slot.permits).acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(CheckoutError::Draining),
                Err(_) => return Err(CheckoutError::QueueTimeout(timeouts.queue)),
            };

        // Drop stale idle connections rather than sending a request into a
        // socket the backend has already closed.
        let now = Instant::now();
        loop {
            let candidate = {
                let mut idle = slot.idle.lock().unwrap_or_else(|p| p.into_inner());
                idle.pop_front()
            };
            match candidate {
                Some(entry) if entry.is_usable(&config, now) => {
                    slot.counters.reused.fetch_add(1, Ordering::Relaxed);
                    return Ok(Lease::new(
                        entry.sender,
                        entry.address,
                        entry.opened,
                        slot,
                        permit,
                        config,
                    ));
                }
                Some(_) => {
                    slot.counters.discarded.fetch_add(1, Ordering::Relaxed);
                }
                None => break,
            }
        }

        let (sender, address) = self.dial(endpoint, timeouts).await?;
        slot.counters.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Lease::new(sender, address, Instant::now(), slot, permit, config))
    }

    pub fn stats(&self) -> Vec<PoolStats> {
        let granted = self.policy().config.max_per_endpoint;
        let endpoints = self.endpoints.lock().unwrap_or_else(|p| p.into_inner());
        let mut stats: Vec<PoolStats> = endpoints
            .iter()
            .map(|(id, slot)| PoolStats {
                endpoint: id.clone(),
                idle: slot.idle.lock().unwrap_or_else(|p| p.into_inner()).len(),
                in_use: granted.saturating_sub(slot.permits.available_permits() as u32),
                waiting: slot.waiting.load(Ordering::Relaxed),
                opened: slot.counters.opened.load(Ordering::Relaxed),
                reused: slot.counters.reused.load(Ordering::Relaxed),
                discarded: slot.counters.discarded.load(Ordering::Relaxed),
            })
            .collect();
        stats.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
        stats
    }

    /// Drop idle connections to addresses an endpoint no longer has.
    ///
    /// Called when a name resolves to something different. Connections to an
    /// address that is still in the set are kept: adding one replica must not
    /// cost every connection to the others.
    ///
    /// In-flight leases are not touched here. They finish, and are then not
    /// parked, because returning one checks the same list.
    pub fn retire(&self, endpoint: &str, keep: &[SocketAddr]) -> usize {
        let endpoints = self.endpoints.lock().unwrap_or_else(|p| p.into_inner());
        let Some(slot) = endpoints.get(endpoint) else {
            return 0;
        };
        *slot.live.lock().unwrap_or_else(|p| p.into_inner()) = keep.to_vec();
        let mut idle = slot.idle.lock().unwrap_or_else(|p| p.into_inner());
        let before = idle.len();
        // A connection with no address is a Unix socket, which no change of
        // addresses can make stale.
        idle.retain(|entry| entry.address.is_none_or(|address| keep.contains(&address)));
        let dropped = before - idle.len();
        slot.counters
            .discarded
            .fetch_add(dropped as u64, Ordering::Relaxed);
        dropped
    }

    /// Stop handing out connections and drop every idle one. In-flight leases
    /// finish normally; their connections are simply not returned.
    pub fn drain(&self) {
        let endpoints = self.endpoints.lock().unwrap_or_else(|p| p.into_inner());
        for slot in endpoints.values() {
            slot.permits.close();
            slot.idle.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
    }
}

struct WaitingGuard {
    slot: Arc<EndpointPool>,
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        self.slot.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
