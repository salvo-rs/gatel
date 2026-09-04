//! `rate-limit`: bound how often one client may ask.
//!
//! The first directive with state that outlives a generation. Its buckets are
//! declared through the state binding rather than kept in a global, which is
//! what makes the reload rules apply to them: raising the limit keeps every
//! client's current allowance, and changing what is counted does not.

use std::any::Any;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, DirectiveState, Phase, Scopes, StateKind};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// What identifies a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// The canonical client address, as the trusted-proxy policy decided it.
    Ip,
    Header(String),
    Cookie(String),
    /// Everything together: a limit on the route rather than on a client.
    Route,
}

impl Key {
    fn describe(&self) -> String {
        match self {
            Key::Ip => "ip".into(),
            Key::Header(name) => format!("header:{name}"),
            Key::Cookie(name) => format!("cookie:{name}"),
            Key::Route => "route".into(),
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "ip" => Ok(Key::Ip),
            "route" => Ok(Key::Route),
            other => match other.split_once(':') {
                Some(("header", name)) if !name.is_empty() => Ok(Key::Header(name.to_ascii_lowercase())),
                Some(("cookie", name)) if !name.is_empty() => Ok(Key::Cookie(name.to_string())),
                _ => Err(format!(
                    "unknown rate limit key `{other}`; use `ip`, `route`, `header:name` or `cookie:name`"
                )),
            },
        }
    }

    /// What to count this request against, or `None` when the request carries
    /// nothing to identify it by.
    fn extract(&self, request: &Request, client_ip: Option<IpAddr>) -> Option<String> {
        match self {
            Key::Ip => client_ip.map(|ip| ip.to_string()),
            Key::Route => Some(String::new()),
            Key::Header(name) => request
                .headers()
                .get(name.as_str())
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            Key::Cookie(name) => {
                let header = request.headers().get(http::header::COOKIE)?.to_str().ok()?;
                header.split(';').find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key.trim() == name).then(|| value.trim().to_string())
                })
            }
        }
    }
}

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(prop)]
    max: u32,
    #[gatel(prop)]
    window: Duration,
    #[gatel(prop, default = String::from("ip"))]
    key: String,
    /// How many distinct clients to remember. A bound is required, or a client
    /// choosing its own key could grow the table without limit.
    #[gatel(prop, default = 100_000)]
    max_keys: u32,
}

#[derive(Debug)]
pub struct Config {
    max: u32,
    window: Duration,
    key: Key,
    max_keys: u32,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max as u64);
        hasher.update_duration(self.window);
        hasher.update_str(&self.key.describe());
        hasher.update_u64(self.max_keys as u64);
    }

    fn state(&self) -> Option<DirectiveState> {
        // Changing what is counted, or over how long, makes existing counts
        // meaningless. Changing only the table size does not.
        let mut compatibility = Hasher::new();
        compatibility.update_str(&self.key.describe());
        compatibility.update_duration(self.window);
        let max_keys = self.max_keys as usize;
        Some(DirectiveState {
            kind: StateKind::RateLimit,
            compatibility: compatibility.finish(),
            create: Box::new(move || Arc::new(Buckets::new(max_keys)) as Arc<dyn Any + Send + Sync>),
        })
    }

    fn build(&self, state: Option<Arc<dyn Any + Send + Sync>>) -> Result<Arc<dyn Handler>, String> {
        let buckets = state
            .and_then(|state| state.downcast::<Buckets>().ok())
            .ok_or_else(|| "rate-limit was not given its buckets".to_string())?;
        Ok(Arc::new(RateLimitHandler {
            buckets,
            max: self.max,
            window: self.window,
            key: self.key.clone(),
        }))
    }

    fn describe(&self) -> String {
        format!(
            "rate-limit {} per {:?} by {}",
            self.max,
            self.window,
            self.key.describe()
        )
    }
}

pub struct RateLimit;

impl Directive for RateLimit {
    fn meta(&self) -> &DirectiveMeta {
        // A guard: it rejects, so it belongs with the other rejections and
        // ahead of anything that would do work on the request.
        static META: DirectiveMeta = DirectiveMeta::middleware("rate-limit", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        if fields.max == 0 {
            diagnostics.push(
                gatel_model::Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`max` must be at least 1",
                )
                .with_help("to refuse everything, use `respond status=403` instead"),
            );
            return None;
        }
        if fields.window.is_zero() {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "`window` must be positive",
            );
            return None;
        }
        let key = match Key::parse(&fields.key) {
            Ok(key) => key,
            Err(message) => {
                diagnostics.error(Code::InvalidValue, node.origin.clone(), message);
                return None;
            }
        };
        Some(Arc::new(Config {
            max: fields.max,
            window: fields.window,
            key,
            max_keys: fields.max_keys.max(1),
        }))
    }
}

/// One client's allowance.
///
/// A token bucket rather than a fixed window: a fixed window lets a client send
/// twice the limit across a boundary, and a client that paces itself should
/// never be refused.
#[derive(Debug)]
struct Bucket {
    /// Tokens scaled by `SCALE`, so refill is exact without floating point.
    tokens: u64,
    last: Instant,
}

const SCALE: u64 = 1_000_000;

/// Every client's allowance, bounded in size.
#[derive(Debug)]
pub struct Buckets {
    inner: Mutex<HashMap<String, Bucket>>,
    max_keys: usize,
    /// How many times the table was cleared because it filled up, so an
    /// operator can tell a limit is being defeated by key churn.
    evictions: AtomicU64,
}

impl Buckets {
    fn new(max_keys: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_keys,
            evictions: AtomicU64::new(0),
        }
    }

    /// Take one token. Returns how long to wait when there is none.
    fn take(&self, key: &str, max: u32, window: Duration) -> Result<u32, Duration> {
        let now = Instant::now();
        let capacity = max as u64 * SCALE;
        let per_nano = capacity as f64 / window.as_nanos().max(1) as f64;

        let mut buckets = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        if !buckets.contains_key(key) && buckets.len() >= self.max_keys {
            // Drop everything that has refilled completely: those entries mean
            // nothing. If that frees nothing, the table is genuinely full and
            // the oldest entries go.
            buckets.retain(|_, bucket| {
                let elapsed = now.duration_since(bucket.last).as_nanos() as f64;
                (bucket.tokens as f64 + elapsed * per_nano) < capacity as f64
            });
            if buckets.len() >= self.max_keys {
                buckets.clear();
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_nanos() as f64;
        bucket.tokens = (bucket.tokens as f64 + elapsed * per_nano).min(capacity as f64) as u64;
        bucket.last = now;

        if bucket.tokens >= SCALE {
            bucket.tokens -= SCALE;
            Ok((bucket.tokens / SCALE) as u32)
        } else {
            let missing = (SCALE - bucket.tokens) as f64;
            Err(Duration::from_nanos((missing / per_nano).ceil() as u64))
        }
    }

    pub fn tracked(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }
}

struct RateLimitHandler {
    buckets: Arc<Buckets>,
    max: u32,
    window: Duration,
    key: Key,
}

#[async_trait]
impl Handler for RateLimitHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let client_ip = depot.get::<IpAddr>("gatel.client_ip").ok().copied();
        let Some(key) = self.key.extract(request, client_ip) else {
            // Nothing to count against. Refusing is the only safe answer: the
            // alternative is an unlimited lane for anyone who omits the header.
            response.status_code(StatusCode::BAD_REQUEST);
            let _ = response.write_body("400 this route requires an identifying header\n");
            ctrl.skip_rest();
            return;
        };

        match self.buckets.take(&key, self.max, self.window) {
            Ok(remaining) => {
                add_header(response, "x-ratelimit-limit", &self.max.to_string());
                add_header(response, "x-ratelimit-remaining", &remaining.to_string());
                ctrl.call_next(request, depot, response).await;
            }
            Err(wait) => {
                let seconds = wait.as_secs().max(1);
                response.status_code(StatusCode::TOO_MANY_REQUESTS);
                add_header(response, "retry-after", &seconds.to_string());
                add_header(response, "x-ratelimit-limit", &self.max.to_string());
                add_header(response, "x-ratelimit-remaining", "0");
                let _ = response.write_body("429 too many requests\n");
                ctrl.skip_rest();
            }
        }
    }
}

fn add_header(response: &mut Response, name: &'static str, value: &str) {
    if let Ok(value) = http::HeaderValue::from_str(value) {
        response
            .headers_mut()
            .insert(http::HeaderName::from_static(name), value);
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = RateLimit.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn node(max: i64, window: &str) -> Node {
        Node::new("rate-limit", origin())
            .with_prop("max", Value::int(max, origin()))
            .with_prop("window", Value::string(window, origin()))
    }

    #[test]
    fn a_limit_needs_both_a_count_and_a_window() {
        let (config, diagnostics) = decode(node(10, "1m"));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert!(config.unwrap().describe().starts_with("rate-limit 10"));

        let bare = Node::new("rate-limit", origin()).with_prop("max", Value::int(10, origin()));
        assert!(decode(bare).0.is_none(), "a limit without a window means nothing");
    }

    #[test]
    fn a_limit_of_zero_points_at_the_directive_that_means_it() {
        let (config, diagnostics) = decode(node(0, "1m"));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("respond")
        );
    }

    #[test]
    fn keys_cover_the_forms_an_operator_needs() {
        assert_eq!(Key::parse("ip").unwrap(), Key::Ip);
        assert_eq!(Key::parse("route").unwrap(), Key::Route);
        assert_eq!(
            Key::parse("header:X-Api-Key").unwrap(),
            Key::Header("x-api-key".into())
        );
        assert_eq!(
            Key::parse("cookie:session").unwrap(),
            Key::Cookie("session".into())
        );
        assert!(Key::parse("header:").is_err());
        assert!(Key::parse("nonsense").is_err());
    }

    #[test]
    fn the_allowance_is_spent_and_then_refused() {
        let buckets = Buckets::new(16);
        let window = Duration::from_secs(60);
        for expected in (0..3).rev() {
            assert_eq!(buckets.take("client", 3, window), Ok(expected));
        }
        assert!(
            buckets.take("client", 3, window).is_err(),
            "the fourth request is over the limit"
        );
    }

    #[test]
    fn the_allowance_refills_over_time_rather_than_in_a_jump() {
        let buckets = Buckets::new(16);
        // 10 per 100ms: one token every 10ms.
        let window = Duration::from_millis(100);
        for _ in 0..10 {
            assert!(buckets.take("client", 10, window).is_ok());
        }
        assert!(buckets.take("client", 10, window).is_err());

        std::thread::sleep(Duration::from_millis(25));
        assert!(
            buckets.take("client", 10, window).is_ok(),
            "some allowance should have returned"
        );
        assert!(buckets.take("client", 10, window).is_ok());
    }

    #[test]
    fn clients_are_counted_separately() {
        let buckets = Buckets::new(16);
        let window = Duration::from_secs(60);
        assert!(buckets.take("a", 1, window).is_ok());
        assert!(buckets.take("a", 1, window).is_err());
        assert!(
            buckets.take("b", 1, window).is_ok(),
            "one client must not spend another's allowance"
        );
    }

    #[test]
    fn the_table_cannot_grow_without_bound() {
        let buckets = Buckets::new(8);
        let window = Duration::from_secs(60);
        for index in 0..100 {
            let _ = buckets.take(&format!("client-{index}"), 5, window);
        }
        assert!(buckets.tracked() <= 8, "tracked {}", buckets.tracked());
        assert!(buckets.evictions() > 0, "filling the table should be visible");
    }

    #[test]
    fn the_wait_it_reports_is_long_enough_to_succeed() {
        let buckets = Buckets::new(16);
        let window = Duration::from_millis(200);
        assert!(buckets.take("client", 1, window).is_ok());
        let wait = buckets.take("client", 1, window).unwrap_err();
        assert!(
            wait <= window,
            "waiting a whole window is always enough: {wait:?}"
        );

        std::thread::sleep(wait + Duration::from_millis(5));
        assert!(buckets.take("client", 1, window).is_ok());
    }

    #[test]
    fn raising_the_limit_keeps_the_buckets_but_changing_the_key_does_not() {
        let of = |key: &str, window: &str, max: i64| {
            let raw = node(max, window).with_prop("key", Value::string(key, origin()));
            decode(raw).0.unwrap().state().unwrap().compatibility
        };
        assert_eq!(
            of("ip", "1m", 10),
            of("ip", "1m", 1000),
            "a bigger allowance is still the same client"
        );
        assert_ne!(of("ip", "1m", 10), of("header:x-key", "1m", 10));
        assert_ne!(
            of("ip", "1m", 10),
            of("ip", "10m", 10),
            "a different window is a different count"
        );
    }

    #[test]
    fn it_guards_rather_than_observes() {
        assert_eq!(RateLimit.meta().role, gatel_plan::Role::Middleware(Phase::Guard));
        assert_eq!(RateLimit.meta().failure_policy, gatel_plan::FailurePolicy::Closed);
    }

    #[test]
    fn building_without_state_fails_rather_than_limiting_nothing() {
        let (config, _) = decode(node(10, "1m"));
        let error = match config.unwrap().build(None) {
            Ok(_) => panic!("a rate limiter with no buckets would let everything through"),
            Err(error) => error,
        };
        assert!(error.contains("buckets"), "{error}");
    }
}
