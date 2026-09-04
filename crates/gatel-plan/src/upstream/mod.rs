//! Upstream configuration.
//!
//! Every limit here is a field with a default rather than a constant in the
//! code, because the right pool size for a gateway in front of two backends is
//! not the right one for a gateway in front of two hundred.

use std::net::SocketAddr;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostics, Hasher, Node, Origin, ResourceId, SourceId};

/// How a request picks among healthy endpoints.
mod decode;

pub use decode::{anonymous_upstream_id, decode_upstream_body};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Balance {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
    Random,
    /// Pick two at random, take the one with fewer requests in flight. Gets
    /// most of the benefit of least-connections without its herd behaviour.
    TwoRandomChoices,
    LeastConn,
    IpHash,
    UriHash,
    HeaderHash {
        header: String,
    },
    CookieHash {
        cookie: String,
    },
    /// Lowest `priority` first; the rest are standby.
    First,
}

/// Read the routing key a hash strategy needs, reporting it as missing rather
/// than quietly hashing something arbitrary.
fn balance_key(node: &Node, strategy: &str, property: &str, diagnostics: &mut Diagnostics) -> Option<String> {
    match node.prop(property) {
        Some(found) => match found.value.as_str() {
            Some(text) if !text.is_empty() => Some(text.to_string()),
            _ => {
                diagnostics.error(
                    Code::InvalidValue,
                    found.origin.clone(),
                    format!("`{property}` must be a non-empty string"),
                );
                None
            }
        },
        None => {
            diagnostics.error(
                Code::MissingField,
                node.origin.clone(),
                format!("`balance \"{strategy}\"` requires the `{property}` property"),
            );
            None
        }
    }
}

impl Balance {
    pub fn parse(name: &str, node: &Node, diagnostics: &mut Diagnostics) -> Option<Self> {
        let strategy = match name {
            "round-robin" => Balance::RoundRobin,
            "weighted-round-robin" => Balance::WeightedRoundRobin,
            "random" => Balance::Random,
            "two-random-choices" => Balance::TwoRandomChoices,
            "least-conn" => Balance::LeastConn,
            "ip-hash" => Balance::IpHash,
            "uri-hash" => Balance::UriHash,
            "header-hash" => Balance::HeaderHash {
                header: balance_key(node, name, "header", diagnostics)?.to_ascii_lowercase(),
            },
            "cookie-hash" => Balance::CookieHash {
                cookie: balance_key(node, name, "cookie", diagnostics)?,
            },
            "first" => Balance::First,
            other => {
                let known = [
                    "round-robin",
                    "weighted-round-robin",
                    "random",
                    "two-random-choices",
                    "least-conn",
                    "ip-hash",
                    "uri-hash",
                    "header-hash",
                    "cookie-hash",
                    "first",
                ];
                let mut diagnostic = gatel_model::Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("unknown balance strategy `{other}`"),
                );
                if let Some(suggestion) = gatel_model::did_you_mean(other, known) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                } else {
                    diagnostic = diagnostic.with_help(format!("known strategies: {}", known.join(", ")));
                }
                diagnostics.push(diagnostic);
                return None;
            }
        };
        // Reject a key on a strategy that ignores it, rather than silently
        // accepting configuration that does nothing.
        for property in &node.props {
            let allowed = matches!(
                (&strategy, property.name.as_str()),
                (Balance::HeaderHash { .. }, "header") | (Balance::CookieHash { .. }, "cookie")
            );
            if !allowed {
                diagnostics.error(
                    Code::UnknownProperty,
                    property.origin.clone(),
                    format!("`balance \"{name}\"` has no property `{}`", property.name),
                );
            }
        }
        Some(strategy)
    }

    pub fn describe(&self) -> String {
        match self {
            Balance::RoundRobin => "round-robin".into(),
            Balance::WeightedRoundRobin => "weighted-round-robin".into(),
            Balance::Random => "random".into(),
            Balance::TwoRandomChoices => "two-random-choices".into(),
            Balance::LeastConn => "least-conn".into(),
            Balance::IpHash => "ip-hash".into(),
            Balance::UriHash => "uri-hash".into(),
            Balance::HeaderHash { header } => format!("header-hash({header})"),
            Balance::CookieHash { cookie } => format!("cookie-hash({cookie})"),
            Balance::First => "first".into(),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

/// One backend address.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// Stable identity. Derived from the address when not written, so
    /// reordering the list never resets health or pool state.
    pub id: String,
    pub address: SocketAddr,
    pub weight: u32,
    pub priority: i32,
    pub origin: Origin,
}

impl Endpoint {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.id);
        hasher.update_str(&self.address.to_string());
        hasher.update_u64(self.weight as u64);
        hasher.update_i64(self.priority as i64);
    }
}

#[derive(Debug, FromNode)]
pub(super) struct EndpointNode {
    #[gatel(arg)]
    address: String,
    #[gatel(prop)]
    id: Option<String>,
    #[gatel(prop, default = 1)]
    weight: u32,
    #[gatel(prop, default = 0)]
    priority: i32,
}

/// Per-stage deadlines for one proxied request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Waiting for a pool slot.
    pub queue: Duration,
    /// TCP connect plus TLS handshake.
    pub connect: Duration,
    /// Sending the request and receiving the response head.
    pub response_head: Duration,
    /// Longest gap between body chunks.
    pub body_idle: Duration,
    /// Whole exchange, including retries.
    pub overall: Duration,
    /// Idle gap inside an upgraded connection.
    pub upgrade_idle: Duration,
    /// Longest an upgraded connection may live.
    pub upgrade_duration: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            queue: Duration::from_secs(2),
            connect: Duration::from_secs(5),
            response_head: Duration::from_secs(10),
            body_idle: Duration::from_secs(60),
            overall: Duration::from_secs(300),
            upgrade_idle: Duration::from_secs(300),
            upgrade_duration: Duration::from_secs(86_400),
        }
    }
}

impl Timeouts {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        for duration in [
            self.queue,
            self.connect,
            self.response_head,
            self.body_idle,
            self.overall,
            self.upgrade_idle,
            self.upgrade_duration,
        ] {
            hasher.update_duration(duration);
        }
    }

    fn validate(&self, origin: &Origin, diagnostics: &mut Diagnostics) {
        let stages = self.queue + self.connect + self.response_head;
        if self.overall < stages {
            diagnostics.error(
                Code::InvalidValue,
                origin.clone(),
                format!(
                    "`overall` ({:?}) is shorter than queue + connect + response-head ({stages:?})",
                    self.overall
                ),
            );
        }
        if self.upgrade_duration < self.upgrade_idle {
            diagnostics.error(
                Code::InvalidValue,
                origin.clone(),
                "`upgrade-duration` must not be shorter than `upgrade-idle`",
            );
        }
    }
}

#[derive(Debug, FromNode)]
pub(super) struct TimeoutsNode {
    #[gatel(prop)]
    queue: Option<Duration>,
    #[gatel(prop)]
    connect: Option<Duration>,
    #[gatel(prop)]
    response_head: Option<Duration>,
    #[gatel(prop)]
    body_idle: Option<Duration>,
    #[gatel(prop)]
    overall: Option<Duration>,
    #[gatel(prop)]
    upgrade_idle: Option<Duration>,
    #[gatel(prop)]
    upgrade_duration: Option<Duration>,
}

/// Connection pool bounds, per upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    pub max_per_endpoint: u32,
    pub max_idle_per_endpoint: u32,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
    pub max_waiters: u32,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            max_per_endpoint: 32,
            max_idle_per_endpoint: 8,
            idle_timeout: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
            max_waiters: 256,
        }
    }
}

impl Pool {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max_per_endpoint as u64);
        hasher.update_u64(self.max_idle_per_endpoint as u64);
        hasher.update_duration(self.idle_timeout);
        hasher.update_duration(self.max_lifetime);
        hasher.update_u64(self.max_waiters as u64);
    }
}

#[derive(Debug, FromNode)]
pub(super) struct PoolNode {
    #[gatel(prop)]
    max_per_endpoint: Option<u32>,
    #[gatel(prop)]
    max_idle_per_endpoint: Option<u32>,
    #[gatel(prop)]
    idle_timeout: Option<Duration>,
    #[gatel(prop)]
    max_lifetime: Option<Duration>,
    #[gatel(prop)]
    max_waiters: Option<u32>,
}

/// How aggressively a failed attempt may be tried elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryMode {
    /// Only failures that happened before any request byte was sent. Safe for
    /// every method, because the upstream cannot have observed the request.
    Safe,
    /// Also replay requests whose body was buffered. Requires an explicit
    /// budget, because it can duplicate a non-idempotent request.
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    pub mode: RetryMode,
    pub attempts: u32,
    pub per_try: Duration,
    pub buffer: ByteSize,
    /// Fraction of in-flight requests that may be retries, as a percentage.
    pub budget_percent: u32,
}

impl Retry {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(match self.mode {
            RetryMode::Safe => "safe",
            RetryMode::Replay => "replay",
        });
        hasher.update_u64(self.attempts as u64);
        hasher.update_duration(self.per_try);
        hasher.update_u64(self.buffer.bytes());
        hasher.update_u64(self.budget_percent as u64);
    }
}

#[derive(Debug, FromNode)]
pub(super) struct RetryNode {
    #[gatel(prop, default = String::from("safe"))]
    mode: String,
    #[gatel(prop, default = 2)]
    attempts: u32,
    #[gatel(prop, default = Duration::from_secs(5))]
    per_try: Duration,
    #[gatel(prop)]
    buffer: Option<ByteSize>,
    #[gatel(prop, default = 20)]
    budget_percent: u32,
}

/// Active health probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub kind: HealthKind,
    pub interval: Duration,
    pub timeout: Duration,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
    pub jitter: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthKind {
    TcpConnect,
    Http {
        path: String,
        status: u16,
        host: Option<String>,
    },
}

impl Health {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        match &self.kind {
            HealthKind::TcpConnect => {
                hasher.update_str("tcp");
            }
            HealthKind::Http { path, status, host } => {
                hasher.update_str("http");
                hasher.update_str(path);
                hasher.update_u64(*status as u64);
                hasher.update_opt_str(host.as_deref());
            }
        }
        hasher.update_duration(self.interval);
        hasher.update_duration(self.timeout);
        hasher.update_u64(self.healthy_threshold as u64);
        hasher.update_u64(self.unhealthy_threshold as u64);
        hasher.update_duration(self.jitter);
    }
}

#[derive(Debug, FromNode)]
pub(super) struct HealthNode {
    #[gatel(arg)]
    kind: String,
    #[gatel(prop)]
    path: Option<String>,
    #[gatel(prop)]
    status: Option<u16>,
    #[gatel(prop)]
    host: Option<String>,
    #[gatel(prop, default = Duration::from_secs(5))]
    interval: Duration,
    #[gatel(prop, default = Duration::from_secs(2))]
    timeout: Duration,
    #[gatel(prop, default = 1)]
    healthy_threshold: u32,
    #[gatel(prop, default = 2)]
    unhealthy_threshold: u32,
    #[gatel(prop)]
    jitter: Option<Duration>,
}

/// Ejecting an endpoint based on live traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassiveHealth {
    pub max_fails: u32,
    pub window: Duration,
    pub cooldown: Duration,
    pub unhealthy_status: Vec<u16>,
}

impl PassiveHealth {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max_fails as u64);
        hasher.update_duration(self.window);
        hasher.update_duration(self.cooldown);
        hasher.update_ordered(&self.unhealthy_status, |status, hasher| {
            hasher.update_u64(*status as u64);
        });
    }
}

#[derive(Debug, FromNode)]
pub(super) struct PassiveHealthNode {
    #[gatel(prop, default = 5)]
    max_fails: u32,
    #[gatel(prop, default = Duration::from_secs(30))]
    window: Duration,
    #[gatel(prop, default = Duration::from_secs(60))]
    cooldown: Duration,
    #[gatel(prop)]
    unhealthy_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpstreamProtocol {
    #[default]
    Http1,
    Http2,
}

/// Upstream TLS. There is deliberately no way to skip verification; use the
/// built-in local CA for development instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTls {
    pub server_name: String,
    pub ca: Option<gatel_model::SecretRef>,
    pub client_cert: Option<gatel_model::SecretRef>,
    pub client_key: Option<gatel_model::SecretRef>,
}

impl UpstreamTls {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.server_name);
        match &self.ca {
            Some(reference) => reference.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        for reference in [&self.client_cert, &self.client_key] {
            match reference {
                Some(reference) => reference.hash_into(hasher),
                None => {
                    hasher.update_none();
                }
            }
        }
    }
}

/// A named or anonymous set of backends with the policy for reaching them.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub id: ResourceId,
    pub owner: SourceId,
    pub endpoints: Vec<Endpoint>,
    pub balance: Balance,
    pub timeouts: Timeouts,
    pub pool: Pool,
    pub retry: Option<Retry>,
    pub health: Option<Health>,
    pub passive_health: Option<PassiveHealth>,
    pub protocol: UpstreamProtocol,
    pub tls: Option<UpstreamTls>,
    pub origin: Origin,
}

impl Upstream {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.id.hash_into(hasher);
        // Endpoint order is not meaningful: two configurations listing the same
        // backends differently must produce the same plan.
        hasher.update_unordered(&self.endpoints, |endpoint, hasher| endpoint.hash_into(hasher));
        self.balance.hash_into(hasher);
        self.timeouts.hash_into(hasher);
        self.pool.hash_into(hasher);
        match &self.retry {
            Some(retry) => retry.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        match &self.health {
            Some(health) => health.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        match &self.passive_health {
            Some(passive) => passive.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        hasher.update_u64(self.protocol as u64);
        match &self.tls {
            Some(tls) => tls.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{ResourceKind, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn upstream_node(children: Vec<Node>) -> Node {
        let mut node = Node::new("upstream", origin());
        node.children = children;
        node
    }

    fn endpoint(address: &str) -> Node {
        Node::new("endpoint", origin()).with_arg(Value::string(address, origin()))
    }

    fn decode(node: &Node) -> (Option<Upstream>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let id = ResourceId::new(ResourceKind::Upstream, "test");
        let upstream = decode_upstream_body(node, id, SourceId::file("main"), &mut diagnostics);
        (upstream, diagnostics)
    }

    #[test]
    fn a_minimal_upstream_gets_sane_defaults() {
        let (upstream, diagnostics) = decode(&upstream_node(vec![endpoint("127.0.0.1:9000")]));
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        let upstream = upstream.unwrap();
        assert_eq!(upstream.balance, Balance::RoundRobin);
        assert_eq!(upstream.endpoints[0].weight, 1);
        assert_eq!(upstream.endpoints[0].id, "127.0.0.1:9000");
        assert_eq!(upstream.timeouts, Timeouts::default());
        assert!(upstream.retry.is_none(), "retry must be off unless asked for");
    }

    #[test]
    fn an_upstream_without_endpoints_is_rejected() {
        let (_, diagnostics) = decode(&upstream_node(vec![]));
        assert!(diagnostics.iter().any(|d| d.code == Code::MissingField));
    }

    #[test]
    fn endpoint_order_does_not_change_the_fingerprint() {
        let forward = decode(&upstream_node(vec![
            endpoint("127.0.0.1:1"),
            endpoint("127.0.0.1:2"),
        ]))
        .0
        .unwrap();
        let reversed = decode(&upstream_node(vec![
            endpoint("127.0.0.1:2"),
            endpoint("127.0.0.1:1"),
        ]))
        .0
        .unwrap();
        let hash = |upstream: &Upstream| {
            let mut hasher = Hasher::new();
            upstream.hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&forward), hash(&reversed));
    }

    #[test]
    fn a_duplicate_endpoint_identity_is_rejected() {
        let (_, diagnostics) = decode(&upstream_node(vec![
            endpoint("127.0.0.1:1"),
            endpoint("127.0.0.1:1"),
        ]));
        assert!(diagnostics.iter().any(|d| d.code == Code::Duplicate));
    }

    #[test]
    fn hash_strategies_require_their_key() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("balance", origin()).with_arg(Value::string("header-hash", origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("requires the `header` property"))
        );

        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("balance", origin())
                .with_arg(Value::string("header-hash", origin()))
                .with_prop("header", Value::string("X-Tenant", origin())),
        ]);
        let (upstream, diagnostics) = decode(&node);
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(
            upstream.unwrap().balance,
            Balance::HeaderHash {
                header: "x-tenant".into()
            }
        );
    }

    #[test]
    fn a_key_on_a_strategy_that_ignores_it_is_rejected() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("balance", origin())
                .with_arg(Value::string("round-robin", origin()))
                .with_prop("header", Value::string("x", origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        assert!(diagnostics.iter().any(|d| d.code == Code::UnknownProperty));
    }

    #[test]
    fn replay_retries_demand_an_explicit_buffer() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("retry", origin()).with_prop("mode", Value::string("replay", origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("requires an explicit `buffer`"))
        );

        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("retry", origin())
                .with_prop("mode", Value::string("replay", origin()))
                .with_prop("buffer", Value::string("1MiB", origin())),
        ]);
        let (upstream, diagnostics) = decode(&node);
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(upstream.unwrap().retry.unwrap().buffer, ByteSize::mib(1));
    }

    #[test]
    fn overall_timeout_must_cover_its_stages() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("timeouts", origin()).with_prop("overall", Value::string("1s", origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("shorter than queue"))
        );
    }

    #[test]
    fn skipping_upstream_certificate_verification_is_not_offered() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("tls", origin())
                .with_prop("server-name", Value::string("api.internal", origin()))
                .with_prop("insecure-skip-verify", Value::bool(true, origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        let diagnostic = diagnostics
            .iter()
            .find(|d| d.code == Code::UnknownProperty)
            .unwrap();
        assert!(diagnostic.help.as_deref().unwrap().contains("cannot be disabled"));
    }

    #[test]
    fn tcp_health_checks_reject_http_only_properties() {
        let node = upstream_node(vec![
            endpoint("127.0.0.1:1"),
            Node::new("health", origin())
                .with_arg(Value::string("tcp", origin()))
                .with_prop("path", Value::string("/healthz", origin())),
        ]);
        let (_, diagnostics) = decode(&node);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("has no property `path`"))
        );
    }

    #[test]
    fn an_unknown_child_node_suggests_a_known_one() {
        let node = upstream_node(vec![endpoint("127.0.0.1:1"), Node::new("timeout", origin())]);
        let (_, diagnostics) = decode(&node);
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("did you mean `timeouts`?")
        );
    }
}
