use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::ser::Serializer;

/// Serialize a `Duration` as a human-readable seconds string (e.g. `"30s"`).
fn serialize_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{}s", d.as_secs()))
}

/// Top-level application configuration parsed from KDL.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppConfig {
    pub global: GlobalConfig,
    pub tls: Option<TlsConfig>,
    pub sites: Vec<SiteConfig>,
    pub stream: Option<StreamConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            global: GlobalConfig::default(),
            tls: None,
            sites: Vec::new(),
            stream: None,
        }
    }
}

/// Global settings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobalConfig {
    pub admin_addr: Option<SocketAddr>,
    pub log_level: String,
    pub log_format: String,
    #[serde(serialize_with = "serialize_duration")]
    pub grace_period: Duration,
    pub http_addr: SocketAddr,
    pub https_addr: SocketAddr,
    /// Enable HTTP/3 (QUIC) listener on the same address as HTTPS.
    /// Only effective when the `http3` feature is enabled and TLS is configured.
    pub http3: bool,
    /// When true, expect PROXY protocol v1/v2 headers on incoming connections.
    pub proxy_protocol: bool,
    pub access_log: Option<LogFileConfig>,
    pub error_log: Option<LogFileConfig>,
    /// TCP_NODELAY socket option for accepted connections (default: true).
    pub tcp_nodelay: bool,
    /// SO_SNDBUF send buffer size hint (bytes). Platform-specific; may be ignored.
    pub tcp_send_buffer: Option<usize>,
    /// SO_RCVBUF receive buffer size hint (bytes). Platform-specific; may be ignored.
    pub tcp_recv_buffer: Option<usize>,
    /// OTLP collector endpoint for OpenTelemetry traces, e.g. `"http://localhost:4317"`.
    /// Only used when gatel is compiled with the `otlp` feature.
    pub otlp_endpoint: Option<String>,
    /// Service name reported in OpenTelemetry traces. Defaults to `"gatel"`.
    pub otlp_service_name: Option<String>,
    /// Bearer token for admin API authentication. When set, all admin API
    /// requests must include `Authorization: Bearer <token>`.
    #[serde(skip_serializing)]
    pub admin_auth_token: Option<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            admin_addr: None,
            log_level: "info".into(),
            log_format: "pretty".into(),
            grace_period: Duration::from_secs(30),
            http_addr: ([0, 0, 0, 0], 80).into(),
            https_addr: ([0, 0, 0, 0], 443).into(),
            http3: false,
            proxy_protocol: false,
            access_log: None,
            error_log: None,
            tcp_nodelay: true,
            tcp_send_buffer: None,
            tcp_recv_buffer: None,
            otlp_endpoint: None,
            otlp_service_name: None,
            admin_auth_token: None,
        }
    }
}

/// Log file configuration for access and error logs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogFileConfig {
    pub path: String,
    pub format: Option<String>,
    pub rotate_size: Option<u64>,   // bytes
    pub rotate_keep: Option<usize>, // number of old files to keep
}

/// Global TLS / ACME settings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TlsConfig {
    pub acme: Option<AcmeConfig>,
    pub client_auth: Option<ClientAuthConfig>,
    pub on_demand: Option<OnDemandTlsConfig>,
    pub min_version: Option<String>,
    pub max_version: Option<String>,
    pub cipher_suites: Vec<String>,
    /// Enable OCSP stapling. For ACME-managed certificates, certon's maintenance
    /// loop already handles OCSP refresh automatically. For manually-loaded
    /// certificates, this flag signals intent; full OCSP stapling for manual
    /// certs requires AIA extension parsing (x509-parser + reqwest).
    pub ocsp_stapling: bool,
    /// ECDH key-exchange curves to enable. Supported values: "x25519",
    /// "secp256r1", "secp384r1". Empty means use rustls defaults.
    pub ecdh_curves: Vec<String>,
}

/// mTLS (mutual TLS) client certificate verification configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClientAuthConfig {
    /// Paths to CA PEM files used to verify client certificates.
    pub ca_certs: Vec<String>,
    /// If true, connections without a valid client certificate are rejected.
    /// If false, client certificates are requested but not required.
    pub required: bool,
}

/// On-demand TLS configuration for automatic certificate issuance at handshake time.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OnDemandTlsConfig {
    /// Optional URL to query to decide whether a domain is allowed.
    /// A GET request is made to `{ask}?domain={sni}`; 200 OK means allowed.
    pub ask: Option<String>,
    /// Maximum number of certificates to issue per minute.
    pub rate_limit: Option<u32>,
}

/// ACME certificate auto-issuance settings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AcmeConfig {
    pub email: String,
    pub ca: CertAuthority,
    pub challenge: ChallengeType,
    /// External Account Binding credentials (required by some CAs like ZeroSSL).
    pub eab: Option<EabConfig>,
    /// DNS provider configuration for DNS-01 challenges.
    pub dns_provider: Option<DnsProviderConfig>,
}

/// External Account Binding (EAB) credentials for ACME registration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EabConfig {
    pub kid: String,
    pub hmac_key: String,
}

/// DNS provider configuration for DNS-01 ACME challenges.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DnsProviderConfig {
    pub provider: String,
    pub api_token: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    /// Extra provider-specific key-value options.
    pub options: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CertAuthority {
    LetsEncrypt,
    LetsEncryptStaging,
    ZeroSsl,
}

impl Default for CertAuthority {
    fn default() -> Self {
        Self::LetsEncrypt
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ChallengeType {
    Http01,
    TlsAlpn01,
    Dns01,
}

impl Default for ChallengeType {
    fn default() -> Self {
        Self::Http01
    }
}

/// Per-site TLS override (manual cert).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SiteTlsConfig {
    pub cert: String,
    pub key: String,
}

/// A virtual host (site) configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SiteConfig {
    pub host: String,
    pub tls: Option<SiteTlsConfig>,
    pub routes: Vec<RouteConfig>,
}

/// A route within a site.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteConfig {
    pub path: String,
    /// Additional matchers that must all pass (in addition to path matching).
    pub matchers: Vec<crate::router::matcher::RequestMatcher>,
    pub middlewares: Vec<HoopConfig>,
    pub handler: HandlerConfig,
    /// Optional condition that must hold for this route to match a request.
    pub condition: Option<RouteCondition>,
}

/// Conditions that can guard a route, evaluated against each incoming request.
#[derive(Debug, Clone, serde::Serialize)]
pub enum RouteCondition {
    /// Match only if the client IP is in one of the given CIDR ranges.
    RemoteIp(Vec<String>),
    /// Match only if the client IP is NOT in any of the given CIDR ranges.
    NotRemoteIp(Vec<String>),
    /// Match only if the named header equals the given value.
    Header { name: String, value: String },
    /// Match only if the named header does NOT equal the given value.
    NotHeader { name: String, value: String },
}

/// Response caching configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheConfig {
    pub max_entries: usize,
    pub max_entry_size: usize,
    #[serde(serialize_with = "serialize_duration")]
    pub default_max_age: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            max_entry_size: 10 * 1024 * 1024, // 10 MB
            default_max_age: Duration::from_secs(300),
        }
    }
}

/// Supported middleware configurations.
#[derive(Debug, Clone, serde::Serialize)]
pub enum HoopConfig {
    RateLimit {
        #[serde(serialize_with = "serialize_duration")]
        window: Duration,
        max: u64,
        /// Maximum burst capacity (token bucket ceiling).  Defaults to `max`
        /// when not specified.
        burst: Option<u64>,
    },
    ForwardAuth {
        /// URL of the authentication service to delegate to.
        url: String,
        /// Headers from the auth response to copy into the original request.
        copy_headers: Vec<String>,
    },
    Encode {
        encodings: Vec<String>,
        level: Option<u32>,
    },
    Headers(HeadersConfig),
    BasicAuth {
        users: Vec<BasicAuthUser>,
        /// Maximum number of consecutive failures before an IP is locked out.
        /// Defaults to 5 when `None`.
        brute_force_max: Option<u32>,
        /// Duration of the lockout period after exceeding `brute_force_max`.
        /// Defaults to 300 seconds when `None`.
        #[serde(
            serialize_with = "serialize_option_duration",
            skip_serializing_if = "Option::is_none"
        )]
        brute_force_window: Option<Duration>,
    },
    IpFilter {
        allow: Vec<String>,
        deny: Vec<String>,
        /// When true, use the first IP in the X-Forwarded-For header instead
        /// of the direct client address.
        forwarded_for: bool,
    },
    Rewrite {
        strip_prefix: Option<String>,
        uri: Option<String>,
        /// Regex rewrite rules: list of (pattern, replacement) pairs applied
        /// sequentially to the path.
        regex_rules: Vec<(String, String)>,
        /// Only rewrite when the resolved path does NOT correspond to an
        /// existing file on disk (requires `root`).
        if_not_file: bool,
        /// Only rewrite when the resolved path does NOT correspond to an
        /// existing directory on disk (requires `root`).
        if_not_dir: bool,
        /// Filesystem root used by `if_not_file` / `if_not_dir` checks.
        root: Option<String>,
        /// When true, collapse consecutive slashes in the path before applying
        /// any other rewrite rules (e.g. `//foo///bar` → `/foo/bar`).
        normalize_slashes: bool,
    },
    Replace {
        /// List of (search, replacement) pairs applied to the response body.
        rules: Vec<(String, String)>,
        /// If true, only replace the first occurrence of each search string.
        once: bool,
    },
    Cache(CacheConfig),
    Templates {
        root: Option<String>,
    },
    BufferLimit {
        /// Maximum allowed request body size in bytes. Returns 413 when exceeded.
        max_request_body: Option<usize>,
        /// Maximum allowed response body size in bytes. Returns 502 when exceeded.
        max_response_body: Option<usize>,
    },
    Cors {
        /// Allowed origins. Use `["*"]` to allow any origin.
        allow_origins: Vec<String>,
        /// Allowed HTTP methods. Use `["*"]` to allow any method.
        allow_methods: Vec<String>,
        /// Allowed request headers. Use `["*"]` to allow any header.
        allow_headers: Vec<String>,
        /// Whether to allow credentials (cookies, authorization headers).
        allow_credentials: bool,
        /// Headers to expose to the browser.
        expose_headers: Vec<String>,
        /// How long (in seconds) the preflight response can be cached.
        max_age: Option<u64>,
    },
    Timeout {
        /// Maximum duration for a request before returning 503.
        #[serde(serialize_with = "serialize_duration")]
        duration: Duration,
    },
    RequestId {
        /// Header name for the request ID. Defaults to `"x-request-id"`.
        header_name: Option<String>,
        /// Whether to overwrite an existing request ID. Defaults to true.
        overwrite: bool,
    },
    ForceHttps {
        /// HTTPS port to redirect to. If not set, uses the default (443).
        https_port: Option<u16>,
    },
    TrailingSlash {
        /// Action: `"add"` or `"remove"`.
        action: String,
    },
    Decompress {
        /// Maximum decompressed body size in bytes. Prevents decompression bombs.
        max_size: Option<usize>,
    },
    ErrorPages {
        /// Maps HTTP status code to custom body content.
        pages: HashMap<u16, String>,
    },
    StreamReplace {
        /// List of (search, replacement) pairs applied to the streaming response body.
        rules: Vec<(String, String)>,
        /// If true, only replace the first occurrence of each search string.
        once: bool,
    },
    /// A module-provided middleware loaded via the plugin system.
    Module {
        /// Module name (matches a registered ModuleLoader).
        name: String,
        /// Module-specific configuration as key-value pairs.
        config: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadersConfig {
    pub request_set: HashMap<String, String>,
    pub response_set: HashMap<String, String>,
    pub response_remove: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BasicAuthUser {
    pub username: String,
    pub password_hash: String,
}

/// Terminal handler for a route.
#[derive(Debug, Clone, serde::Serialize)]
pub enum HandlerConfig {
    Proxy(ProxyConfig),
    FastCgi(FastCgiConfig),
    ForwardProxy(ForwardProxyConfig),
    Cgi(CgiConfig),
    Scgi(ScgiConfig),
    FileServer(FileServerConfig),
    Redirect {
        to: String,
        permanent: bool,
    },
    Respond {
        status: u16,
        body: String,
    },
    /// A module-provided handler loaded via the plugin system.
    Module {
        name: String,
        config: HashMap<String, String>,
    },
}

/// Forward proxy handler configuration (HTTP CONNECT tunneling).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForwardProxyConfig {
    /// Optional list of (username, password_hash) pairs.  When non-empty,
    /// incoming CONNECT requests must supply a valid `Proxy-Authorization:
    /// Basic` header or receive a 407 response.
    pub auth_users: Vec<BasicAuthUser>,
}

/// CGI handler configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CgiConfig {
    /// Filesystem root from which CGI scripts are resolved.
    pub root: String,
    /// Extra environment variables injected into every CGI invocation.
    pub env: HashMap<String, String>,
}

/// SCGI handler configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScgiConfig {
    /// Address of the SCGI server, e.g. `"127.0.0.1:9000"`.
    pub addr: String,
    /// Extra environment variables injected into every request.
    pub env: HashMap<String, String>,
}

/// FastCGI handler configuration (e.g. for PHP-FPM).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FastCgiConfig {
    /// Address of the FastCGI server, e.g. `"127.0.0.1:9000"`.
    pub addr: String,
    /// Document root on the FastCGI server.
    pub script_root: String,
    /// Index filenames for directory requests.
    pub index: Vec<String>,
    /// Path-info split marker, e.g. `".php"`.
    pub split_path: Option<String>,
    /// Extra environment variables.
    pub env: HashMap<String, String>,
}

/// Reverse proxy handler config.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProxyConfig {
    pub upstreams: Vec<UpstreamConfig>,
    pub lb: LbPolicy,
    /// Header name used by `HeaderHash` load balancing.
    pub lb_header: Option<String>,
    /// Cookie name used by `CookieHash` load balancing.
    pub lb_cookie: Option<String>,
    pub health_check: Option<HealthCheckConfig>,
    pub passive_health: Option<PassiveHealthConfig>,
    pub headers_up: HashMap<String, String>,
    pub headers_down: HashMap<String, String>,
    /// Number of retry attempts on upstream failure (0 = no retries).
    pub retries: u32,
    /// Optional DNS-based dynamic upstream resolution.
    pub dynamic_upstreams: Option<DnsUpstreamConfig>,
    /// Custom response bodies to substitute when upstream returns these status
    /// codes. Maps HTTP status code to the response body string.
    pub error_pages: HashMap<u16, String>,
    /// Regex-based header replacement rules applied to upstream request headers.
    /// Each entry is `(header_name, pattern, replacement)`.
    pub headers_up_replace: Vec<(String, String, String)>,
    /// When true, skip TLS certificate verification for HTTPS upstreams.
    pub tls_skip_verify: bool,
    /// When true, use HTTP/2 exclusively when communicating with upstreams.
    pub upstream_http2: bool,
    /// Maximum total concurrent connections to upstreams. Returns 503 when exceeded.
    pub max_connections: Option<usize>,
    /// Idle keepalive timeout for upstream connections.
    #[serde(serialize_with = "serialize_option_duration")]
    pub keepalive_timeout: Option<Duration>,
    /// When true (default), normalize the URI path before forwarding by
    /// collapsing consecutive slashes and resolving `.` / `..` segments.
    /// Set to false to forward the URI exactly as received.
    pub sanitize_uri: bool,
    /// Optional SRV-record-based dynamic upstream resolution.
    pub srv_upstream: Option<SrvUpstreamConfig>,
}

fn serialize_option_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(dur) => s.serialize_some(&format!("{}s", dur.as_secs())),
        None => s.serialize_none(),
    }
}

/// DNS-based dynamic upstream resolution configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DnsUpstreamConfig {
    /// DNS name to resolve, e.g. `"app.svc.cluster.local"`.
    pub dns_name: String,
    /// Port to pair with resolved IPs.
    pub port: u16,
    /// How often to re-resolve the DNS name.
    #[serde(serialize_with = "serialize_duration")]
    pub refresh_interval: Duration,
}

/// SRV-record-based dynamic upstream resolution configuration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SrvUpstreamConfig {
    /// DNS SRV service name, e.g. `"_http._tcp.example.com"`.
    pub service_name: String,
    /// How often to re-resolve the SRV records.
    #[serde(serialize_with = "serialize_duration")]
    pub refresh_interval: Duration,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UpstreamConfig {
    pub addr: String,
    pub weight: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum LbPolicy {
    #[default]
    RoundRobin,
    Random,
    WeightedRoundRobin,
    IpHash,
    LeastConn,
    UriHash,
    HeaderHash,
    CookieHash,
    First,
    TwoRandomChoices,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthCheckConfig {
    pub uri: String,
    #[serde(serialize_with = "serialize_duration")]
    pub interval: Duration,
    #[serde(serialize_with = "serialize_duration")]
    pub timeout: Duration,
    pub unhealthy_threshold: u32,
    pub healthy_threshold: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PassiveHealthConfig {
    pub max_fails: u32,
    #[serde(serialize_with = "serialize_duration")]
    pub fail_window: Duration,
    #[serde(serialize_with = "serialize_duration")]
    pub cooldown: Duration,
}

impl Default for PassiveHealthConfig {
    fn default() -> Self {
        Self {
            max_fails: 5,
            fail_window: Duration::from_secs(30),
            cooldown: Duration::from_secs(60),
        }
    }
}

/// Static file serving config.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileServerConfig {
    pub root: String,
    pub browse: bool,
    /// When true (default), redirect directory requests without trailing slash to add one.
    pub trailing_slash: bool,
    /// List of index filenames to try when serving a directory (default: ["index.html"]).
    pub index: Vec<String>,
}

/// L4 TCP stream proxy config.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamConfig {
    pub listeners: Vec<StreamListenerConfig>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamListenerConfig {
    pub listen: SocketAddr,
    pub proxy: String,
}
