//! The resource graph: what the operator asked for, with every frontend
//! detail already gone.
//!
//! A `Spec` is immutable and source-agnostic. Its fingerprint answers exactly
//! one question — "did the operator ask for something different?" — which is
//! what lets an unchanged reload stop before doing any work.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use gatel_model::{ByteSize, Fingerprint, Hasher, Origin, ResourceId, SecretRef, SourceId};

use crate::directive::DirectiveInstance;
use crate::matcher::{HostMatch, Matcher, MethodMatch, PathMatch};
use crate::upstream::Upstream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerProtocol {
    Http,
    Https,
}

impl ListenerProtocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            ListenerProtocol::Http => "http",
            ListenerProtocol::Https => "https",
        }
    }

    pub const fn is_tls(self) -> bool {
        matches!(self, ListenerProtocol::Https)
    }
}

/// A socket the gateway listens on.
#[derive(Debug, Clone)]
pub struct Listener {
    pub id: ResourceId,
    pub owner: SourceId,
    /// The name sites refer to with `listener=`.
    pub name: String,
    pub bind: SocketAddr,
    pub protocol: ListenerProtocol,
    pub origin: Origin,
}

impl Listener {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.id.hash_into(hasher);
        hasher.update_str(&self.name);
        hasher.update_str(&self.bind.to_string());
        hasher.update_str(self.protocol.as_str());
    }
}

/// Where a site's certificate comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateSource {
    /// PEM files named by the configuration.
    Manual { cert: SecretRef, key: SecretRef },
    /// The built-in development CA, trusted with `gatel trust`.
    Internal,
    /// Issued and renewed automatically.
    Acme,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteTls {
    pub source: CertificateSource,
    pub client_auth: Option<ClientAuth>,
    /// True when nothing in the configuration said `tls`, and a name that can
    /// only ever be local was given a development certificate so it could be
    /// served at all.
    ///
    /// Deliberately not hashed: writing `tls internal` and having it inferred
    /// are the same request, and `spec_hash` answers "did the operator ask for
    /// something different?".
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAuth {
    /// `false` still verifies a certificate if one is offered; it only allows
    /// anonymous clients through.
    pub required: bool,
    pub ca: SecretRef,
}

impl SiteTls {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        match &self.source {
            CertificateSource::Manual { cert, key } => {
                hasher.update_str("manual");
                cert.hash_into(hasher);
                key.hash_into(hasher);
            }
            CertificateSource::Internal => {
                hasher.update_str("internal");
            }
            CertificateSource::Acme => {
                hasher.update_str("acme");
            }
        }
        match &self.client_auth {
            Some(auth) => {
                hasher.update_bool(auth.required);
                auth.ca.hash_into(hasher);
            }
            None => {
                hasher.update_none();
            }
        }
    }
}

/// A route's single terminal.
#[derive(Debug, Clone)]
pub enum Terminal {
    /// Reverse proxy. Owned by the kernel rather than a bundle, because
    /// endpoint selection, health and connection reuse are one correctness
    /// chain that must not be replaceable piecemeal.
    Proxy { upstream: ResourceId },
    /// Any other terminal, supplied by a bundle.
    Directive(DirectiveInstance),
}

impl Terminal {
    pub fn name(&self) -> &str {
        match self {
            Terminal::Proxy { .. } => "proxy",
            Terminal::Directive(instance) => instance.meta.name,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Terminal::Proxy { upstream } => format!("proxy -> {upstream}"),
            Terminal::Directive(instance) => instance.config.describe(),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        match self {
            Terminal::Proxy { upstream } => {
                hasher.update_str("proxy");
                upstream.hash_into(hasher);
            }
            Terminal::Directive(instance) => {
                hasher.update_str("directive");
                instance.hash_into(hasher);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub id: ResourceId,
    pub path: PathMatch,
    pub methods: MethodMatch,
    pub matchers: Vec<Matcher>,
    /// Sorted by phase, then order, then name. Never by declaration position.
    pub directives: Vec<DirectiveInstance>,
    pub terminal: Terminal,
    pub origin: Origin,
}

impl Route {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.id.hash_into(hasher);
        self.path.hash_into(hasher);
        self.methods.hash_into(hasher);
        hasher.update_ordered(&self.matchers, |matcher, hasher| matcher.hash_into(hasher));
        hasher.update_ordered(&self.directives, |instance, hasher| instance.hash_into(hasher));
        self.terminal.hash_into(hasher);
    }
}

#[derive(Debug, Clone)]
pub struct Site {
    pub id: ResourceId,
    pub owner: SourceId,
    pub host: HostMatch,
    /// Listener names this site is served on. Empty means every listener whose
    /// protocol suits the site.
    pub listeners: Vec<String>,
    pub tls: Option<SiteTls>,
    pub directives: Vec<DirectiveInstance>,
    pub routes: Vec<Route>,
    pub origin: Origin,
}

impl Site {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.id.hash_into(hasher);
        self.host.hash_into(hasher);
        hasher.update_unordered(&self.listeners, |name, hasher| {
            hasher.update_str(name);
        });
        match &self.tls {
            Some(tls) => tls.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        hasher.update_ordered(&self.directives, |instance, hasher| instance.hash_into(hasher));
        // Routes are ordered by specificity during normalization, so their
        // order here is meaningful.
        hasher.update_ordered(&self.routes, |route, hasher| route.hash_into(hasher));
    }
}

/// Process-wide protection limits. Every one of these is a configuration field
/// with a default rather than a constant compiled into the data plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The largest request head that will be read, in bytes.
    pub max_header: ByteSize,
    /// The largest request body that will be accepted.
    pub max_body: ByteSize,
    /// How long a TLS handshake may take. A plaintext listener has none.
    pub handshake_timeout: Duration,
    /// How long a connection may go without a complete request head arriving.
    ///
    /// One number covers two things that look identical from the socket: a
    /// keep-alive connection waiting for its next request, and a client
    /// dribbling a head out one byte at a time. Splitting them would need to
    /// know which the peer intends, which nothing does.
    pub idle_timeout: Duration,
    /// How many connections the process will hold open at once, across every
    /// listener.
    pub max_connections: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header: ByteSize::kib(64),
            max_body: ByteSize::mib(16),
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(60),
            max_connections: 10_000,
        }
    }
}

impl Limits {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.max_header.bytes());
        hasher.update_u64(self.max_body.bytes());
        hasher.update_duration(self.handshake_timeout);
        hasher.update_duration(self.idle_timeout);
        hasher.update_u64(self.max_connections as u64);
    }
}

/// Where run-time logging goes. The environment overrides this, so an
/// operator can raise the level without editing and reloading configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub level: String,
    /// `text`, `json` or `clf`.
    pub format: String,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "text".to_string(),
        }
    }
}

/// Process-wide settings that are not themselves resources.
#[derive(Debug, Clone)]
pub struct Global {
    pub grace_period: Duration,
    pub limits: Limits,
    /// Peers whose forwarding headers are believed.
    pub trusted_proxies: Vec<(IpAddr, u8)>,
    pub admin: Option<Admin>,
    /// Present exactly when the configuration asked for automatic
    /// certificates. A site that says `tls acme` without it is an error.
    pub acme: Option<Acme>,
    pub log: Log,
}

impl Default for Global {
    fn default() -> Self {
        Self {
            grace_period: Duration::from_secs(30),
            limits: Limits::default(),
            trusted_proxies: Vec::new(),
            admin: None,
            acme: None,
            log: Log::default(),
        }
    }
}

impl Global {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_duration(self.grace_period);
        self.limits.hash_into(hasher);
        // Logging does not change what a request receives, but it does change
        // what the process does, so a reload that only touches it is still a
        // real change.
        hasher.update_str(&self.log.level);
        hasher.update_str(&self.log.format);
        hasher.update_unordered(&self.trusted_proxies, |(network, prefix), hasher| {
            hasher.update_str(&network.to_string());
            hasher.update_u64(*prefix as u64);
        });
        match &self.admin {
            Some(admin) => admin.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
        match &self.acme {
            Some(acme) => acme.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
    }
}

/// Automatic certificates, for every site that asks for them.
///
/// Process-wide rather than per site, because the account with the CA is: one
/// key, one contact, one set of rate limits. A site says `tls acme` and nothing
/// else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acme {
    /// The CA's directory URL.
    pub directory: String,
    /// The contact the CA writes to about expiry and problems. Required,
    /// because a certificate nobody can be told about is one that expires
    /// silently.
    pub email: String,
    /// Whether the operator has accepted the CA's terms. Never defaulted to
    /// true: agreeing on somebody's behalf is not this program's to do.
    pub agreed: bool,
    /// Where the account key and issued certificates are kept.
    pub directory_path: Option<String>,
    /// Renew once this much of the certificate's life is left.
    pub renew_before: Duration,
}

impl Acme {
    /// Let's Encrypt, which is what an operator who did not say means.
    pub const DEFAULT_DIRECTORY: &'static str = "https://acme-v02.api.letsencrypt.org/directory";

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.directory);
        hasher.update_str(&self.email);
        hasher.update_bool(self.agreed);
        hasher.update_opt_str(self.directory_path.as_deref());
        hasher.update_duration(self.renew_before);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admin {
    /// A unix socket path or a loopback socket address.
    pub bind: AdminBind,
    pub token: Option<SecretRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminBind {
    Unix(String),
    Tcp(SocketAddr),
}

impl Admin {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        match &self.bind {
            AdminBind::Unix(path) => {
                hasher.update_str("unix");
                hasher.update_str(path);
            }
            AdminBind::Tcp(address) => {
                hasher.update_str("tcp");
                hasher.update_str(&address.to_string());
            }
        }
        match &self.token {
            Some(token) => token.hash_into(hasher),
            None => {
                hasher.update_none();
            }
        }
    }
}

/// The whole configuration, normalized.
#[derive(Debug, Clone)]
pub struct Spec {
    pub global: Global,
    pub listeners: Vec<Listener>,
    pub sites: Vec<Site>,
    pub upstreams: Vec<Upstream>,
}

impl Spec {
    /// "Did the operator ask for something different?"
    ///
    /// Comments, whitespace, declaration order and file layout are all absent
    /// by construction, so a cosmetic edit produces the same value.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut hasher = Hasher::new();
        hasher.update_str("gatel.spec.v1");
        self.global.hash_into(&mut hasher);
        hasher.update_unordered(&self.listeners, |listener, hasher| listener.hash_into(hasher));
        hasher.update_unordered(&self.sites, |site, hasher| site.hash_into(hasher));
        hasher.update_unordered(&self.upstreams, |upstream, hasher| upstream.hash_into(hasher));
        hasher.finish()
    }

    pub fn listener(&self, name: &str) -> Option<&Listener> {
        self.listeners.iter().find(|listener| listener.name == name)
    }

    pub fn upstream(&self, id: &ResourceId) -> Option<&Upstream> {
        self.upstreams.iter().find(|upstream| &upstream.id == id)
    }

    pub fn route_count(&self) -> usize {
        self.sites.iter().map(|site| site.routes.len()).sum()
    }

    /// Which listeners a site is actually served on.
    pub fn listeners_for(&self, site: &Site) -> Vec<&Listener> {
        if site.listeners.is_empty() {
            let wants_tls = site.tls.is_some();
            self.listeners
                .iter()
                .filter(|listener| listener.protocol.is_tls() == wants_tls)
                .collect()
        } else {
            site.listeners
                .iter()
                .filter_map(|name| self.listener(name))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::ResourceKind;

    use super::*;

    fn listener(name: &str, bind: &str) -> Listener {
        Listener {
            id: ResourceId::new(ResourceKind::Listener, name),
            owner: SourceId::file("main"),
            name: name.to_string(),
            bind: bind.parse().unwrap(),
            protocol: if name == "https" {
                ListenerProtocol::Https
            } else {
                ListenerProtocol::Http
            },
            origin: Origin::synthetic("test"),
        }
    }

    fn site(host: &str, listeners: Vec<String>, tls: bool) -> Site {
        Site {
            id: ResourceId::new(ResourceKind::Site, host),
            owner: SourceId::file("main"),
            host: HostMatch::parse(host).unwrap(),
            listeners,
            tls: tls.then_some(SiteTls {
                automatic: false,
                source: CertificateSource::Internal,
                client_auth: None,
            }),
            directives: Vec::new(),
            routes: Vec::new(),
            origin: Origin::synthetic("test"),
        }
    }

    fn spec(listeners: Vec<Listener>, sites: Vec<Site>) -> Spec {
        Spec {
            global: Global::default(),
            listeners,
            sites,
            upstreams: Vec::new(),
        }
    }

    #[test]
    fn listener_and_site_order_do_not_change_the_fingerprint() {
        let forward = spec(
            vec![listener("public", "0.0.0.0:80"), listener("https", "0.0.0.0:443")],
            vec![
                site("a.example.com", vec![], false),
                site("b.example.com", vec![], false),
            ],
        );
        let reversed = spec(
            vec![listener("https", "0.0.0.0:443"), listener("public", "0.0.0.0:80")],
            vec![
                site("b.example.com", vec![], false),
                site("a.example.com", vec![], false),
            ],
        );
        assert_eq!(forward.fingerprint(), reversed.fingerprint());
    }

    #[test]
    fn changing_a_bind_address_changes_the_fingerprint() {
        let before = spec(vec![listener("public", "0.0.0.0:80")], vec![]);
        let after = spec(vec![listener("public", "0.0.0.0:8080")], vec![]);
        assert_ne!(before.fingerprint(), after.fingerprint());
    }

    #[test]
    fn a_site_without_an_explicit_listener_uses_matching_protocols() {
        let spec = spec(
            vec![listener("public", "0.0.0.0:80"), listener("https", "0.0.0.0:443")],
            vec![
                site("plain.example.com", vec![], false),
                site("secure.example.com", vec![], true),
            ],
        );
        let plain = spec.listeners_for(&spec.sites[0]);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].name, "public");

        let secure = spec.listeners_for(&spec.sites[1]);
        assert_eq!(secure.len(), 1);
        assert_eq!(secure[0].name, "https");
    }

    #[test]
    fn an_explicit_listener_wins_over_the_protocol_default() {
        let spec = spec(
            vec![listener("public", "0.0.0.0:80"), listener("https", "0.0.0.0:443")],
            vec![site("a.example.com", vec!["https".into()], false)],
        );
        let chosen = spec.listeners_for(&spec.sites[0]);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].name, "https");
    }

    #[test]
    fn global_limits_take_part_in_the_fingerprint() {
        let mut changed = spec(vec![], vec![]);
        let baseline = changed.fingerprint();
        changed.global.limits.max_body = ByteSize::mib(1);
        assert_ne!(changed.fingerprint(), baseline);
    }
}
