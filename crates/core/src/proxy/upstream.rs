use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dashmap::DashMap;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use super::dns_upstream::{DnsResolver, DynamicUpstreamConfig};
use super::srv_upstream::SrvResolver;
use crate::Body;
use crate::config::ProxyConfig;

/// A no-op TLS certificate verifier that accepts any certificate.
/// Used when `tls_skip_verify` is enabled.
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A single backend server.
#[derive(Debug, Clone)]
pub struct Backend {
    pub addr: String,
    pub weight: u32,
    pub activity_key: Option<String>,
}

#[derive(Clone)]
struct BackendState {
    healthy: Arc<AtomicBool>,
    active_conns: Arc<AtomicUsize>,
}

impl BackendState {
    fn new() -> Self {
        Self {
            healthy: Arc::new(AtomicBool::new(true)),
            active_conns: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// A backend plus its shared health and connection counters.
#[derive(Clone)]
pub struct BackendEntry {
    pub backend: Backend,
    healthy: Arc<AtomicBool>,
    active_conns: Arc<AtomicUsize>,
}

impl BackendEntry {
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn set_healthy(&self, val: bool) {
        self.healthy.store(val, Ordering::Relaxed);
    }

    pub fn conn_count(&self) -> usize {
        self.active_conns.load(Ordering::Relaxed)
    }

    pub fn acquire_conn(&self) -> ConnGuard {
        self.active_conns.fetch_add(1, Ordering::Relaxed);
        ConnGuard {
            active_conns: Some(Arc::clone(&self.active_conns)),
        }
    }
}

/// A consistent view of the currently available static and dynamic backends.
#[derive(Clone)]
pub struct BackendSnapshot {
    entries: Vec<BackendEntry>,
}

impl BackendSnapshot {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&BackendEntry> {
        self.entries.get(idx)
    }

    pub fn entries(&self) -> &[BackendEntry] {
        &self.entries
    }

    pub fn is_healthy(&self, idx: usize) -> bool {
        self.get(idx).map(BackendEntry::is_healthy).unwrap_or(false)
    }

    pub fn set_healthy(&self, idx: usize, val: bool) {
        if let Some(entry) = self.get(idx) {
            entry.set_healthy(val);
        }
    }

    pub fn conn_count(&self, idx: usize) -> usize {
        self.get(idx)
            .map(BackendEntry::conn_count)
            .unwrap_or(usize::MAX)
    }

    pub fn acquire_conn(&self, idx: usize) -> ConnGuard {
        self.get(idx)
            .map(BackendEntry::acquire_conn)
            .unwrap_or_else(ConnGuard::noop)
    }

    pub fn total_active_conns(&self) -> usize {
        self.entries.iter().map(BackendEntry::conn_count).sum()
    }
}

/// Pool of upstream backends with a shared HTTP client, health status,
/// and active-connection counters.
pub struct UpstreamPool {
    static_backends: Vec<Backend>,
    states: DashMap<String, BackendState>,
    dns_resolver: Option<DnsResolver>,
    srv_resolver: Option<SrvResolver>,
    pub client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>,
    /// Optional total connection limit across all backends.
    pub max_connections: Option<usize>,
}

impl UpstreamPool {
    pub fn from_config(config: &ProxyConfig) -> Self {
        let static_backends: Vec<Backend> = config
            .upstreams
            .iter()
            .map(|u| Backend {
                addr: u.addr.clone(),
                weight: u.weight,
                activity_key: u.activity_key.clone(),
            })
            .collect();

        // Build the HTTPS connector — handles both HTTP and HTTPS upstreams.
        let connector = if config.tls_skip_verify {
            // Build a rustls ClientConfig that skips certificate verification.
            let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("default protocol versions are valid")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();

            hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(tls_config)
                .https_or_http()
                .enable_http1()
                .enable_http2()
                .build()
        } else {
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_provider_and_webpki_roots(Arc::new(rustls::crypto::ring::default_provider()))
                .expect("default protocol versions are valid")
                .https_or_http()
                .enable_http1()
                .enable_http2()
                .build()
        };

        let mut builder = Client::builder(TokioExecutor::new());

        if config.upstream_http2 {
            builder.http2_only(true);
        }

        if let Some(timeout) = config.keepalive_timeout {
            builder.pool_idle_timeout(timeout);
        }

        let client = builder.build(connector);
        let dns_resolver = config.dynamic_upstreams.as_ref().map(|dynamic| {
            DnsResolver::start(&DynamicUpstreamConfig {
                dns_name: dynamic.dns_name.clone(),
                port: dynamic.port,
                refresh_interval: dynamic.refresh_interval,
            })
        });
        let srv_resolver = config
            .srv_upstream
            .as_ref()
            .map(|srv| SrvResolver::start(srv.service_name.clone(), srv.refresh_interval));

        Self {
            static_backends,
            states: DashMap::new(),
            dns_resolver,
            srv_resolver,
            client,
            max_connections: config.max_connections,
        }
    }

    pub fn snapshot(&self) -> BackendSnapshot {
        let mut backends = self.static_backends.clone();

        if let Some(resolver) = &self.dns_resolver {
            backends.extend(resolver.backends.load().iter().cloned());
        }
        if let Some(resolver) = &self.srv_resolver {
            backends.extend(resolver.backends.load().iter().cloned());
        }

        let mut seen = HashSet::new();
        let entries: Vec<BackendEntry> = backends
            .into_iter()
            .filter(|backend| seen.insert(backend.addr.clone()))
            .map(|backend| {
                let state = self
                    .states
                    .entry(backend.addr.clone())
                    .or_insert_with(BackendState::new)
                    .clone();
                BackendEntry {
                    backend,
                    healthy: state.healthy,
                    active_conns: state.active_conns,
                }
            })
            .collect();

        BackendSnapshot { entries }
    }

    #[cfg(test)]
    fn set_dns_backends_for_test(&self, backends: Vec<Backend>) {
        self.dns_resolver
            .as_ref()
            .expect("DNS resolver is configured")
            .backends
            .store(backends);
    }

    #[cfg(test)]
    fn set_srv_backends_for_test(&self, backends: Vec<Backend>) {
        self.srv_resolver
            .as_ref()
            .expect("SRV resolver is configured")
            .backends
            .store(backends);
    }

    /// Returns `true` if the backend at `idx` is currently marked healthy.
    pub fn is_healthy(&self, idx: usize) -> bool {
        self.snapshot().is_healthy(idx)
    }

    /// Mark a backend as healthy or unhealthy.
    pub fn set_healthy(&self, idx: usize, val: bool) {
        self.snapshot().set_healthy(idx, val);
    }

    /// Increment the active connection count for a backend. Returns a guard
    /// that decrements on drop.
    pub fn acquire_conn(&self, idx: usize) -> ConnGuard {
        self.snapshot().acquire_conn(idx)
    }

    /// Get the current active connection count for a backend.
    pub fn conn_count(&self, idx: usize) -> usize {
        self.snapshot().conn_count(idx)
    }

    /// Total active connections across all backends.
    pub fn total_active_conns(&self) -> usize {
        self.snapshot().total_active_conns()
    }

    /// Number of backends.
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// Whether the pool has no backends.
    pub fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }
}

/// RAII guard that decrements the active-connection counter on drop.
pub struct ConnGuard {
    active_conns: Option<Arc<AtomicUsize>>,
}

impl ConnGuard {
    fn noop() -> Self {
        Self { active_conns: None }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(c) = self.active_conns.take() {
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::config::{
        DnsUpstreamConfig, LbPolicy, ProxyConfig, SrvUpstreamConfig, UpstreamConfig,
    };

    fn proxy_config(upstreams: Vec<UpstreamConfig>) -> ProxyConfig {
        ProxyConfig {
            upstreams,
            lb: LbPolicy::RoundRobin,
            lb_header: None,
            lb_cookie: None,
            health_check: None,
            passive_health: None,
            headers_up: HashMap::new(),
            headers_down: HashMap::new(),
            retries: 0,
            dynamic_upstreams: None,
            error_pages: HashMap::new(),
            headers_up_replace: Vec::new(),
            tls_skip_verify: false,
            upstream_http2: false,
            max_connections: None,
            keepalive_timeout: None,
            sanitize_uri: true,
            srv_upstream: None,
        }
    }

    fn upstream(addr: &str, weight: u32) -> UpstreamConfig {
        UpstreamConfig {
            addr: addr.to_string(),
            weight,
            activity_key: None,
        }
    }

    fn backend(addr: &str, weight: u32) -> Backend {
        Backend {
            addr: addr.to_string(),
            weight,
            activity_key: None,
        }
    }

    #[test]
    fn snapshot_preserves_state_by_backend_address() {
        let pool = UpstreamPool::from_config(&proxy_config(vec![
            upstream("127.0.0.1:3000", 1),
            upstream("127.0.0.1:3001", 1),
        ]));
        let snapshot = pool.snapshot();

        snapshot.set_healthy(1, false);
        let guard = snapshot.acquire_conn(1);

        let refreshed = pool.snapshot();
        assert!(refreshed.is_healthy(0));
        assert!(!refreshed.is_healthy(1));
        assert_eq!(refreshed.conn_count(1), 1);

        drop(guard);
        assert_eq!(pool.snapshot().conn_count(1), 0);
    }

    #[test]
    fn snapshot_deduplicates_backends_by_address() {
        let pool = UpstreamPool::from_config(&proxy_config(vec![
            upstream("127.0.0.1:3000", 1),
            upstream("127.0.0.1:3000", 10),
        ]));

        let snapshot = pool.snapshot();

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get(0).unwrap().backend.weight, 1);
    }

    #[tokio::test]
    async fn snapshot_merges_static_dns_and_srv_backends() {
        let mut config = proxy_config(vec![upstream("127.0.0.1:3000", 1)]);
        config.dynamic_upstreams = Some(DnsUpstreamConfig {
            dns_name: "dynamic.invalid".to_string(),
            port: 8080,
            refresh_interval: Duration::from_secs(3600),
        });
        config.srv_upstream = Some(SrvUpstreamConfig {
            service_name: "_http._tcp.dynamic.invalid".to_string(),
            refresh_interval: Duration::from_secs(3600),
        });
        let pool = UpstreamPool::from_config(&config);

        pool.set_dns_backends_for_test(vec![backend("127.0.0.1:3001", 1)]);
        pool.set_srv_backends_for_test(vec![backend("svc.example:8080", 5)]);

        let snapshot = pool.snapshot();
        let addrs: Vec<&str> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.backend.addr.as_str())
            .collect();

        assert_eq!(
            addrs,
            vec!["127.0.0.1:3000", "127.0.0.1:3001", "svc.example:8080"]
        );
        assert_eq!(snapshot.get(2).unwrap().backend.weight, 5);
    }
}
