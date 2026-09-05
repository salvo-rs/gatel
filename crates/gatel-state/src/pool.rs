//! The upstream connection pool.
//!
//! Bounded on purpose, and bounded by configuration rather than by a constant:
//! a gateway in front of two backends and one in front of two hundred want
//! very different numbers. Capacity is a semaphore, so waiting for a slot is
//! ordinary backpressure rather than an unbounded queue, and the wait is
//! covered by the `queue` timeout.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gatel_plan::{Timeouts, UpstreamProtocol};
use http_body_util::combinators::BoxBody;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::endpoint::Endpoint;

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

enum Sender {
    Http1(http1::SendRequest<Body>),
    /// HTTP/2 multiplexes, so one sender serves many concurrent requests and
    /// is cloned rather than checked out exclusively.
    Http2(http2::SendRequest<Body>),
}

struct Idle {
    sender: Sender,
    /// Which address this connection actually reached. Kept so that when a
    /// name stops resolving to an address, the sockets to it can be dropped
    /// and the ones to addresses it still has can be left alone.
    address: SocketAddr,
    opened: Instant,
    returned: Instant,
}

impl Idle {
    fn is_usable(&self, config: &gatel_plan::Pool, now: Instant) -> bool {
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
struct Counters {
    opened: AtomicU64,
    reused: AtomicU64,
    discarded: AtomicU64,
}

struct EndpointPool {
    permits: Arc<Semaphore>,
    /// How many permits the semaphore has been given. The semaphore itself
    /// only reports what is *available*, which is not the same number while
    /// requests are in flight, so the total has to be remembered to resize it.
    granted: AtomicU32,
    idle: Mutex<VecDeque<Idle>>,
    waiting: AtomicU64,
    counters: Counters,
    /// The addresses this endpoint currently has, or empty for "everything is
    /// still current". Read when a connection is returned, so one that reached
    /// a replaced container is dropped rather than parked.
    live: Mutex<Vec<SocketAddr>>,
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
                    return Ok(Lease {
                        sender: Some(entry.sender),
                        address: entry.address,
                        opened: entry.opened,
                        slot,
                        permit,
                        config,
                        reusable: true,
                    });
                }
                Some(_) => {
                    slot.counters.discarded.fetch_add(1, Ordering::Relaxed);
                }
                None => break,
            }
        }

        let (sender, address) = self.dial(endpoint, timeouts).await?;
        slot.counters.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Lease {
            sender: Some(sender),
            address,
            opened: Instant::now(),
            slot,
            permit,
            config,
            reusable: true,
        })
    }

    /// Reach the backend, and say which of its addresses answered.
    ///
    /// `timeouts.connect` bounds the whole of this - resolving the name if it
    /// has not been resolved yet, and every address tried - so a name with four
    /// dead addresses cannot take four times as long as one dead address does.
    async fn dial(
        &self,
        endpoint: &Endpoint,
        timeouts: &Timeouts,
    ) -> Result<(Sender, SocketAddr), CheckoutError> {
        let deadline = Instant::now() + timeouts.connect;
        let addresses = self.addresses_for(endpoint, deadline).await?;

        let mut last: Option<CheckoutError> = None;
        for address in addresses {
            let Some(budget) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self.dial_one(address, budget).await {
                Ok(sender) => return Ok((sender, address)),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or(CheckoutError::ConnectTimeout(timeouts.connect)))
    }

    /// The addresses to try, resolving on demand when a name has none yet.
    ///
    /// The resolver task fills this in the background, so this path is only
    /// reached in the moments after a start - but it has to exist, or the first
    /// request after a start would fail for a reason that has nothing to do
    /// with the backend.
    async fn addresses_for(
        &self,
        endpoint: &Endpoint,
        deadline: Instant,
    ) -> Result<Vec<SocketAddr>, CheckoutError> {
        let ordered = endpoint.dial_order();
        if !ordered.is_empty() {
            return Ok(ordered);
        }
        let budget = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| CheckoutError::Resolve("no time left to look the name up".to_string()))?;
        let resolve = self.policy().resolve;
        let policy = gatel_plan::Resolve {
            timeout: budget.min(resolve.timeout),
            ..resolve
        };
        match crate::resolve::lookup(&endpoint.target, policy).await {
            Ok(addresses) => {
                endpoint.set_addresses(addresses);
                Ok(endpoint.dial_order())
            }
            Err(error) => Err(CheckoutError::Resolve(error)),
        }
    }

    async fn dial_one(&self, address: SocketAddr, budget: Duration) -> Result<Sender, CheckoutError> {
        let stream = match tokio::time::timeout(budget, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(CheckoutError::Connect(format!("{address}: {error}"))),
            Err(_) => return Err(CheckoutError::ConnectTimeout(budget)),
        };
        // Interactive traffic; batching small writes only adds latency.
        let _ = stream.set_nodelay(true);

        // The handshake is bounded by the same deadline as the connection it
        // is part of. A backend that accepts and then says nothing must not
        // hold a slot for longer than dialling one would have.
        match &self.tls {
            None => self.handshake(TokioIo::new(stream)).await,
            Some(tls) => {
                let secured = match tokio::time::timeout(budget, tls.connect(stream)).await {
                    Ok(Ok(secured)) => secured,
                    Ok(Err(error)) => return Err(CheckoutError::Connect(error)),
                    Err(_) => return Err(CheckoutError::ConnectTimeout(budget)),
                };
                self.handshake(TokioIo::new(secured)).await
            }
        }
    }

    /// The HTTP handshake, over whichever transport got here.
    async fn handshake<IO>(&self, io: TokioIo<IO>) -> Result<Sender, CheckoutError>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        match self.protocol {
            UpstreamProtocol::Http1 => {
                let (sender, connection) = http1::handshake(io)
                    .await
                    .map_err(|error| CheckoutError::Handshake(error.to_string()))?;
                // The driver owns the socket for the connection's lifetime and
                // ends when the sender is dropped and the peer closes.
                tokio::spawn(async move {
                    if let Err(error) = connection.with_upgrades().await {
                        tracing::debug!(target: "gatel::upstream", %error, "connection closed");
                    }
                });
                Ok(Sender::Http1(sender))
            }
            UpstreamProtocol::Http2 => {
                let (sender, connection) = http2::handshake(TokioExecutor::new(), io)
                    .await
                    .map_err(|error| CheckoutError::Handshake(error.to_string()))?;
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        tracing::debug!(target: "gatel::upstream", %error, "connection closed");
                    }
                });
                Ok(Sender::Http2(sender))
            }
        }
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
        idle.retain(|entry| keep.contains(&entry.address));
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

/// A borrowed connection. Returned to the pool on drop unless something made it
/// unusable.
pub struct Lease {
    sender: Option<Sender>,
    address: SocketAddr,
    opened: Instant,
    slot: Arc<EndpointPool>,
    permit: OwnedSemaphorePermit,
    config: gatel_plan::Pool,
    reusable: bool,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("age", &self.opened.elapsed())
            .field("reusable", &self.reusable)
            .finish()
    }
}

impl Lease {
    /// Send a request. Consumes the lease's exclusive access for HTTP/1.1 and
    /// clones the multiplexed sender for HTTP/2.
    pub async fn send(
        &mut self,
        request: http::Request<Body>,
    ) -> Result<http::Response<hyper::body::Incoming>, hyper::Error> {
        let result = match self.sender.as_mut() {
            Some(Sender::Http1(sender)) => sender.send_request(request).await,
            Some(Sender::Http2(sender)) => sender.send_request(request).await,
            None => unreachable!("a lease always holds a sender until it is dropped"),
        };
        if result.is_err() {
            self.reusable = false;
        }
        result
    }

    /// Mark the connection as not reusable — after an upgrade, or when the
    /// response made further requests on it unsafe.
    pub fn poison(&mut self) {
        self.reusable = false;
    }

    /// Whether the underlying connection accepts another request now. HTTP/2
    /// senders are ready as long as the connection is open.
    pub fn is_ready(&self) -> bool {
        match self.sender.as_ref() {
            Some(Sender::Http1(sender)) => sender.is_ready(),
            Some(Sender::Http2(sender)) => !sender.is_closed(),
            None => false,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else { return };
        let closed = match &sender {
            Sender::Http1(sender) => sender.is_closed(),
            Sender::Http2(sender) => sender.is_closed(),
        };
        let now = Instant::now();
        let expired = now.duration_since(self.opened) >= self.config.max_lifetime;
        if !self.reusable || closed || expired {
            return;
        }
        // A connection to an address the name has since stopped resolving to
        // reached a container that is being replaced. Parking it would hand the
        // next request a socket to something on its way out.
        {
            let live = self.slot.live.lock().unwrap_or_else(|p| p.into_inner());
            if !live.is_empty() && !live.contains(&self.address) {
                return;
            }
        }
        let mut idle = self.slot.idle.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() >= self.config.max_idle_per_endpoint as usize {
            // Keeping more idle sockets than configured wastes file
            // descriptors on the backend as much as here.
            return;
        }
        idle.push_back(Idle {
            sender,
            address: self.address,
            opened: self.opened,
            returned: now,
        });
        // The permit is released after the connection is parked, so a waiter
        // that wakes up immediately finds it.
        let _ = &self.permit;
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use gatel_model::{Origin, ResourceId, ResourceKind, SourceId};
    use gatel_plan::Upstream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::endpoint::EndpointSet;

    /// A backend that answers a fixed response and keeps the connection open.
    async fn backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    while let Ok(read) = stream.read(&mut buffer).await {
                        if read == 0 {
                            return;
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await;
                    }
                });
            }
        });
        address
    }

    fn set(address: SocketAddr, config: gatel_plan::Pool) -> (EndpointSet, ConnectionPool) {
        let upstream = Upstream {
            id: ResourceId::new(ResourceKind::Upstream, "api"),
            owner: SourceId::file("main"),
            endpoints: vec![gatel_plan::Endpoint {
                id: "a".into(),
                target: gatel_plan::Target::Address(address),
                weight: 1,
                priority: 0,
                origin: Origin::synthetic("test"),
            }],
            balance: Default::default(),
            timeouts: Default::default(),
            pool: config,
            retry: None,
            health: None,
            passive_health: None,
            protocol: Default::default(),
            tls: None,
            resolve: Default::default(),
            origin: Origin::synthetic("test"),
        };
        let pool = ConnectionPool::new(config, UpstreamProtocol::Http1, Default::default(), None);
        (EndpointSet::new(&upstream), pool)
    }

    fn request() -> http::Request<Body> {
        use http_body_util::{BodyExt, Empty};
        http::Request::builder()
            .uri("/")
            .header("host", "backend")
            .body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())
            .unwrap()
    }

    #[tokio::test]
    async fn a_connection_is_reused_rather_than_reopened() {
        let address = backend().await;
        let (set, pool) = set(address, gatel_plan::Pool::default());
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts::default();

        for _ in 0..3 {
            let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
            let response = lease.send(request()).await.unwrap();
            assert_eq!(response.status(), 200);
        }
        let stats = &pool.stats()[0];
        assert_eq!(
            stats.opened, 1,
            "one socket should have served all three requests"
        );
        assert_eq!(stats.reused, 2);
    }

    #[tokio::test]
    async fn capacity_is_bounded_and_waiting_times_out() {
        let address = backend().await;
        let config = gatel_plan::Pool {
            max_per_endpoint: 1,
            ..gatel_plan::Pool::default()
        };
        let (set, pool) = set(address, config);
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts {
            queue: Duration::from_millis(50),
            ..Timeouts::default()
        };

        let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
        let blocked = pool.checkout(&endpoint, &timeouts).await;
        assert!(
            matches!(blocked, Err(CheckoutError::QueueTimeout(_))),
            "{blocked:?}"
        );

        drop(held);
        assert!(
            pool.checkout(&endpoint, &timeouts).await.is_ok(),
            "the slot must be released"
        );
    }

    #[tokio::test]
    async fn too_many_waiters_are_refused_rather_than_queued_without_limit() {
        let address = backend().await;
        let config = gatel_plan::Pool {
            max_per_endpoint: 1,
            max_waiters: 1,
            ..gatel_plan::Pool::default()
        };
        let (set, pool) = set(address, config);
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts {
            queue: Duration::from_millis(200),
            ..Timeouts::default()
        };

        let _held = pool.checkout(&endpoint, &timeouts).await.unwrap();
        let first = pool.checkout(&endpoint, &timeouts);
        let second = pool.checkout(&endpoint, &timeouts);
        let (a, b) = tokio::join!(first, second);
        let refused = [a, b]
            .into_iter()
            .filter(|result| matches!(result, Err(CheckoutError::QueueFull { .. })))
            .count();
        assert_eq!(refused, 1, "exactly one of the two waiters is over the limit");
    }

    #[tokio::test]
    async fn connecting_to_a_dead_address_reports_a_pre_send_failure() {
        // Take a port and release it, so nothing is listening on an address
        // that is certainly reachable. Using a documentation address instead
        // would depend on what the local network does with it.
        let address = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let (set, pool) = set(address, gatel_plan::Pool::default());
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts {
            connect: Duration::from_millis(200),
            ..Timeouts::default()
        };

        let error = pool.checkout(&endpoint, &timeouts).await.unwrap_err();
        assert!(
            error.is_pre_send(),
            "a failure to connect is always safe to retry elsewhere"
        );
        assert!(
            matches!(
                error,
                CheckoutError::ConnectTimeout(_) | CheckoutError::Connect(_)
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn idle_connections_beyond_the_limit_are_closed() {
        let address = backend().await;
        let config = gatel_plan::Pool {
            max_per_endpoint: 4,
            max_idle_per_endpoint: 1,
            ..gatel_plan::Pool::default()
        };
        let (set, pool) = set(address, config);
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts::default();

        let mut leases = Vec::new();
        for _ in 0..3 {
            let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
            lease.send(request()).await.unwrap();
            leases.push(lease);
        }
        drop(leases);
        assert_eq!(pool.stats()[0].idle, 1, "only one connection may be parked");
    }

    #[tokio::test]
    async fn a_poisoned_lease_is_not_returned() {
        let address = backend().await;
        let (set, pool) = set(address, gatel_plan::Pool::default());
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts::default();

        let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
        lease.send(request()).await.unwrap();
        lease.poison();
        drop(lease);
        assert_eq!(pool.stats()[0].idle, 0);
    }

    #[tokio::test]
    async fn draining_closes_idle_connections_and_refuses_new_ones() {
        let address = backend().await;
        let (set, pool) = set(address, gatel_plan::Pool::default());
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts::default();

        let mut lease = pool.checkout(&endpoint, &timeouts).await.unwrap();
        lease.send(request()).await.unwrap();
        drop(lease);
        assert_eq!(pool.stats()[0].idle, 1);

        pool.drain();
        assert_eq!(pool.stats()[0].idle, 0);
        assert!(matches!(
            pool.checkout(&endpoint, &timeouts).await,
            Err(CheckoutError::Draining)
        ));
    }

    #[tokio::test]
    async fn raising_a_limit_takes_effect_on_a_pool_the_reload_kept() {
        // Sizing is not part of what makes a pool reusable, so a raised limit
        // reaches a pool that already exists. Before `adopt` it reached
        // nothing: the numbers were read once, at creation, and a reload that
        // kept the pool kept the old ones for as long as the process ran.
        let address = backend().await;
        let (set, pool) = set(
            address,
            gatel_plan::Pool {
                max_per_endpoint: 1,
                ..gatel_plan::Pool::default()
            },
        );
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts {
            queue: Duration::from_millis(100),
            ..Timeouts::default()
        };

        let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
        assert!(
            matches!(
                pool.checkout(&endpoint, &timeouts).await,
                Err(CheckoutError::QueueTimeout(_))
            ),
            "one permit means one connection"
        );

        pool.adopt(
            gatel_plan::Pool {
                max_per_endpoint: 2,
                ..gatel_plan::Pool::default()
            },
            gatel_plan::Resolve::default(),
        );

        let second = pool.checkout(&endpoint, &timeouts).await;
        assert!(second.is_ok(), "{second:?}");
        drop(held);
    }

    #[tokio::test]
    async fn lowering_a_limit_gives_back_what_nobody_is_holding() {
        let address = backend().await;
        let (set, pool) = set(
            address,
            gatel_plan::Pool {
                max_per_endpoint: 4,
                ..gatel_plan::Pool::default()
            },
        );
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts {
            queue: Duration::from_millis(100),
            ..Timeouts::default()
        };

        let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
        pool.adopt(
            gatel_plan::Pool {
                max_per_endpoint: 1,
                ..gatel_plan::Pool::default()
            },
            gatel_plan::Resolve::default(),
        );

        // The one in flight is not taken away from the request holding it, but
        // nothing new may start beside it.
        assert!(
            matches!(
                pool.checkout(&endpoint, &timeouts).await,
                Err(CheckoutError::QueueTimeout(_))
            ),
            "a lowered limit must bind immediately for anything new"
        );
        drop(held);
    }

    #[tokio::test]
    async fn what_a_pool_reports_in_use_follows_the_limit_it_now_has() {
        let address = backend().await;
        let (set, pool) = set(
            address,
            gatel_plan::Pool {
                max_per_endpoint: 4,
                ..gatel_plan::Pool::default()
            },
        );
        let endpoint = Arc::clone(&set.all()[0]);
        let timeouts = Timeouts::default();

        let held = pool.checkout(&endpoint, &timeouts).await.unwrap();
        assert_eq!(pool.stats()[0].in_use, 1);

        pool.adopt(
            gatel_plan::Pool {
                max_per_endpoint: 8,
                ..gatel_plan::Pool::default()
            },
            gatel_plan::Resolve::default(),
        );
        assert_eq!(
            pool.stats()[0].in_use,
            1,
            "raising the ceiling does not change how many are in use"
        );
        drop(held);
    }
}
