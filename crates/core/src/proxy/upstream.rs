use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

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

/// Pool of upstream backends with a shared HTTP client, health status,
/// and active-connection counters.
pub struct UpstreamPool {
    pub backends: Vec<Backend>,
    pub client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>,
    /// Per-backend health flag. `true` = healthy, `false` = unhealthy.
    pub healthy: Vec<AtomicBool>,
    /// Per-backend active connection count (used by LeastConn).
    pub active_conns: Arc<Vec<AtomicUsize>>,
    /// Optional total connection limit across all backends.
    pub max_connections: Option<usize>,
}

impl UpstreamPool {
    pub fn from_config(config: &ProxyConfig) -> Self {
        let backends: Vec<Backend> = config
            .upstreams
            .iter()
            .map(|u| Backend {
                addr: u.addr.clone(),
                weight: u.weight,
                activity_key: u.activity_key.clone(),
            })
            .collect();

        let n = backends.len();
        let healthy: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(true)).collect();
        let active_conns: Arc<Vec<AtomicUsize>> =
            Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect());

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
            // Build a standard rustls ClientConfig with an empty root store.
            // Upstreams on HTTPS should use certificates trusted by the system,
            // but since we don't have native-roots or webpki-roots features enabled
            // here, we use an empty store — suitable for internal/self-signed CAs
            // when skip-verify is not set. For production use with public CAs,
            // enable the webpki-roots or native-roots feature on hyper-rustls.
            let root_store = rustls::RootCertStore::empty();
            let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("default protocol versions are valid")
            .with_root_certificates(root_store)
            .with_no_client_auth();

            hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(tls_config)
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

        Self {
            backends,
            client,
            healthy,
            active_conns,
            max_connections: config.max_connections,
        }
    }

    /// Returns `true` if the backend at `idx` is currently marked healthy.
    pub fn is_healthy(&self, idx: usize) -> bool {
        self.healthy
            .get(idx)
            .map(|h| h.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Mark a backend as healthy or unhealthy.
    pub fn set_healthy(&self, idx: usize, val: bool) {
        if let Some(h) = self.healthy.get(idx) {
            h.store(val, Ordering::Relaxed);
        }
    }

    /// Increment the active connection count for a backend. Returns a guard
    /// that decrements on drop.
    pub fn acquire_conn(&self, idx: usize) -> ConnGuard {
        if let Some(c) = self.active_conns.get(idx) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        ConnGuard {
            active_conns: Arc::clone(&self.active_conns),
            idx,
        }
    }

    /// Get the current active connection count for a backend.
    pub fn conn_count(&self, idx: usize) -> usize {
        self.active_conns
            .get(idx)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(usize::MAX)
    }

    /// Total active connections across all backends.
    pub fn total_active_conns(&self) -> usize {
        self.active_conns
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Number of backends.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Whether the pool has no backends.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

/// RAII guard that decrements the active-connection counter on drop.
pub struct ConnGuard {
    active_conns: Arc<Vec<AtomicUsize>>,
    idx: usize,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(c) = self.active_conns.get(self.idx) {
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
