use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::HeaderMap;

use super::upstream::UpstreamPool;

// ---------------------------------------------------------------------------
// Context passed to load balancers that need request-level information
// ---------------------------------------------------------------------------

/// Contextual information about the current request, made available to
/// load-balancing strategies that need more than the pool itself.
pub struct LbContext {
    pub client_addr: SocketAddr,
    pub uri: String,
    pub headers: HeaderMap,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Selects an upstream backend index from the pool.
///
/// Implementations must be `Send + Sync` so they can be shared across tasks.
pub trait LoadBalancer: Send + Sync {
    /// Choose a backend index.  Returns `None` when no backend is available.
    fn select(&self, pool: &UpstreamPool, ctx: &LbContext) -> Option<usize>;
}

// ---------------------------------------------------------------------------
// Round-Robin
// ---------------------------------------------------------------------------

/// Simple round-robin load balancer.
pub struct RoundRobinLb {
    counter: AtomicUsize,
}

impl RoundRobinLb {
    pub fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
        }
    }
}

impl LoadBalancer for RoundRobinLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        let n = pool.len();
        if n == 0 {
            return None;
        }
        // Try up to `n` times to find a healthy backend.
        for _ in 0..n {
            let idx = self.counter.fetch_add(1, Ordering::Relaxed) % n;
            if pool.is_healthy(idx) {
                return Some(idx);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Random
// ---------------------------------------------------------------------------

/// Random selection across healthy backends.
pub struct RandomLb;

impl RandomLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for RandomLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        use rand::prelude::IndexedRandom;

        let healthy_indices: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        if healthy_indices.is_empty() {
            return None;
        }
        let mut rng = rand::rng();
        healthy_indices.choose(&mut rng).copied()
    }
}

// ---------------------------------------------------------------------------
// Weighted Round-Robin (smooth, nginx-style)
// ---------------------------------------------------------------------------

/// Smooth weighted round-robin as described in the nginx implementation.
///
/// Each backend has:
///   - `effective_weight` (starts equal to the configured weight; can be reduced on transient
///     errors and recovered later).
///   - `current_weight` (accumulates each round; the backend with the highest current_weight is
///     selected, then has total_weight subtracted).
pub struct WeightedRoundRobinLb {
    state: Mutex<Vec<WrrEntry>>,
}

struct WrrEntry {
    effective_weight: i64,
    current_weight: i64,
}

impl WeightedRoundRobinLb {
    pub fn new(weights: &[u32]) -> Self {
        let state = weights
            .iter()
            .map(|&w| WrrEntry {
                effective_weight: w as i64,
                current_weight: 0,
            })
            .collect();
        Self {
            state: Mutex::new(state),
        }
    }
}

impl LoadBalancer for WeightedRoundRobinLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        let mut entries = self.state.lock().ok()?;
        if entries.is_empty() {
            return None;
        }

        let mut total: i64 = 0;
        let mut best_idx: Option<usize> = None;
        let mut best_weight: i64 = i64::MIN;

        for (i, entry) in entries.iter_mut().enumerate() {
            if !pool.is_healthy(i) {
                continue;
            }
            entry.current_weight += entry.effective_weight;
            total += entry.effective_weight;

            if entry.current_weight > best_weight {
                best_weight = entry.current_weight;
                best_idx = Some(i);
            }
        }

        if let Some(idx) = best_idx {
            entries[idx].current_weight -= total;
        }

        best_idx
    }
}

// ---------------------------------------------------------------------------
// IP-Hash
// ---------------------------------------------------------------------------

/// Select a backend by hashing the client IP address for session affinity.
pub struct IpHashLb;

impl IpHashLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for IpHashLb {
    fn select(&self, pool: &UpstreamPool, ctx: &LbContext) -> Option<usize> {
        let healthy: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        if healthy.is_empty() {
            return None;
        }
        let hash = hash_value(&ctx.client_addr.ip().to_string());
        Some(healthy[hash as usize % healthy.len()])
    }
}

// ---------------------------------------------------------------------------
// Least Connections
// ---------------------------------------------------------------------------

/// Select the healthy backend with the fewest active connections.
pub struct LeastConnLb;

impl LeastConnLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for LeastConnLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        let mut best_idx: Option<usize> = None;
        let mut best_count = usize::MAX;

        for i in 0..pool.len() {
            if !pool.is_healthy(i) {
                continue;
            }
            let count = pool.conn_count(i);
            if count < best_count {
                best_count = count;
                best_idx = Some(i);
            }
        }

        best_idx
    }
}

// ---------------------------------------------------------------------------
// URI-Hash
// ---------------------------------------------------------------------------

/// Select a backend by hashing the request URI path.  Requests to the same
/// path will consistently hit the same backend (useful for caching).
pub struct UriHashLb;

impl UriHashLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for UriHashLb {
    fn select(&self, pool: &UpstreamPool, ctx: &LbContext) -> Option<usize> {
        let healthy: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        if healthy.is_empty() {
            return None;
        }
        let hash = hash_value(&ctx.uri);
        Some(healthy[hash as usize % healthy.len()])
    }
}

// ---------------------------------------------------------------------------
// Header-Hash
// ---------------------------------------------------------------------------

/// Select a backend by hashing the value of a specific request header.
pub struct HeaderHashLb {
    header_name: String,
}

impl HeaderHashLb {
    pub fn new(header_name: String) -> Self {
        Self { header_name }
    }
}

impl LoadBalancer for HeaderHashLb {
    fn select(&self, pool: &UpstreamPool, ctx: &LbContext) -> Option<usize> {
        let healthy: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        if healthy.is_empty() {
            return None;
        }

        let value = ctx
            .headers
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let hash = hash_value(value);
        Some(healthy[hash as usize % healthy.len()])
    }
}

// ---------------------------------------------------------------------------
// Cookie-Hash
// ---------------------------------------------------------------------------

/// Select a backend by hashing the value of a specific cookie.
pub struct CookieHashLb {
    cookie_name: String,
}

impl CookieHashLb {
    pub fn new(cookie_name: String) -> Self {
        Self { cookie_name }
    }
}

impl LoadBalancer for CookieHashLb {
    fn select(&self, pool: &UpstreamPool, ctx: &LbContext) -> Option<usize> {
        let healthy: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        if healthy.is_empty() {
            return None;
        }

        let cookie_value = extract_cookie(&ctx.headers, &self.cookie_name).unwrap_or_default();
        let hash = hash_value(&cookie_value);
        Some(healthy[hash as usize % healthy.len()])
    }
}

// ---------------------------------------------------------------------------
// First (always pick the first healthy backend)
// ---------------------------------------------------------------------------

/// Always select the first healthy backend.  Simple active/standby failover.
pub struct FirstLb;

impl FirstLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for FirstLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        (0..pool.len()).find(|&i| pool.is_healthy(i))
    }
}

// ---------------------------------------------------------------------------
// Two Random Choices
// ---------------------------------------------------------------------------

/// Two Random Choices (Power of Two Choices) load balancer.
///
/// Picks two healthy backends at random and selects the one with fewer active
/// connections. This achieves near-optimal load distribution with O(1)
/// selection cost, avoiding the global scan of `LeastConnLb`.
///
/// - 0 healthy backends → `None`
/// - 1 healthy backend  → use it directly
/// - 2+ healthy backends → pick 2 at random, choose the one with fewer connections
pub struct TwoRandomChoicesLb;

impl TwoRandomChoicesLb {
    pub fn new() -> Self {
        Self
    }
}

impl LoadBalancer for TwoRandomChoicesLb {
    fn select(&self, pool: &UpstreamPool, _ctx: &LbContext) -> Option<usize> {
        use rand::prelude::IndexedRandom;

        let healthy: Vec<usize> = (0..pool.len()).filter(|&i| pool.is_healthy(i)).collect();
        match healthy.len() {
            0 => None,
            1 => Some(healthy[0]),
            _ => {
                let mut rng = rand::rng();
                // Sample two distinct candidates.
                let candidates: Vec<usize> = healthy.sample(&mut rng, 2).copied().collect();
                let a = candidates[0];
                let b = candidates[1];
                // Pick the backend with fewer active connections.
                if pool.conn_count(a) <= pool.conn_count(b) {
                    Some(a)
                } else {
                    Some(b)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute a deterministic hash for a string value using `DefaultHasher`.
fn hash_value(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Extract a cookie value from the `Cookie` header(s).
fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(http::header::COOKIE) {
        let Ok(cookie_str) = value.to_str() else {
            continue;
        };
        for pair in cookie_str.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == name {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}
