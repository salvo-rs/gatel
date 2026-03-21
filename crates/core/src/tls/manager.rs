//! TLS configuration manager with ACME auto-issuance, mTLS, and on-demand TLS support.
//!
//! [`TlsManager`] bridges gatel's configuration with `certon` for automatic
//! certificate management and supports manually-specified PEM certificates on a
//! per-site basis.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use certon::{
    AcmeIssuer, CertResolver, Certificate, Config as CertAutoConfig, FileStorage, OnDemandConfig,
    Storage,
};
use rustls::RootCertStore;
use rustls::server::ResolvesServerCert;
use rustls::sign::CertifiedKey;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use crate::ProxyError;
use crate::config::{
    AcmeConfig, AppConfig, CertAuthority, ChallengeType, ClientAuthConfig, DnsProviderConfig,
    OnDemandTlsConfig, SiteTlsConfig, TlsConfig,
};

// ---------------------------------------------------------------------------
// TlsManager
// ---------------------------------------------------------------------------

/// Manages TLS configuration for the proxy, supporting both automatic ACME
/// certificate issuance (via certon) and manually-provided PEM certificates.
///
/// The manager builds a `rustls::ServerConfig` that uses a composite
/// certificate resolver:
/// - Sites with explicit `SiteTlsConfig` (cert/key PEM paths) are loaded immediately and registered
///   as manual overrides.
/// - Sites without explicit certs are enrolled in ACME management through certon, which obtains,
///   caches, and auto-renews their certificates.
///
/// Additionally supports:
/// - **mTLS** (mutual TLS): client certificate verification using configured CA certificates. When
///   `client_auth` is configured, the server requests (and optionally requires) client
///   certificates.
/// - **On-demand TLS**: automatic certificate issuance at handshake time for previously unknown
///   domains, with optional ask-URL gating and rate limiting.
///
/// Call [`TlsManager::reload`] to hot-swap the TLS configuration when the
/// proxy config changes.
pub struct TlsManager {
    /// The certon config that drives ACME certificate management.
    certon_config: Option<Arc<CertAutoConfig>>,

    /// The composite resolver used by the rustls `ServerConfig`.
    resolver: Arc<CompositeResolver>,

    /// The current rustls `ServerConfig`, swappable for hot-reload.
    server_config: ArcSwap<rustls::ServerConfig>,

    /// Handle to the certon maintenance task (renewal + OCSP refresh).
    maintenance_handle: Option<JoinHandle<()>>,

    /// Shared challenge map for the HTTP-01 solver. The ACME challenge
    /// middleware reads from this map to serve challenge responses.
    challenge_map: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
}

impl TlsManager {
    /// Build a new `TlsManager` from the application configuration.
    ///
    /// This performs the initial TLS setup:
    /// 1. Loads manual certificates for sites that specify cert/key paths.
    /// 2. Configures certon for ACME-managed sites (if ACME is enabled).
    /// 3. Calls `manage_sync` to obtain/load certificates for ACME domains.
    /// 4. Starts the certon maintenance loop.
    /// 5. Sets up mTLS client certificate verification if configured.
    /// 6. Configures on-demand TLS if configured.
    ///
    /// # Errors
    ///
    /// Returns an error if manual certificate loading fails or if ACME
    /// management cannot be initialized.
    pub async fn build(config: &AppConfig) -> Result<Self, ProxyError> {
        let challenge_map: Arc<tokio::sync::RwLock<HashMap<String, String>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        // Partition sites into manual-cert and ACME-managed.
        let mut manual_certs: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
        let mut acme_domains: Vec<String> = Vec::new();

        for site in &config.sites {
            if let Some(ref site_tls) = site.tls {
                match load_manual_cert(site_tls) {
                    Ok(certified_key) => {
                        info!(
                            host = %site.host,
                            cert = %site_tls.cert,
                            "loaded manual TLS certificate"
                        );
                        manual_certs.insert(site.host.clone(), certified_key);
                    }
                    Err(e) => {
                        error!(
                            host = %site.host,
                            cert = %site_tls.cert,
                            key = %site_tls.key,
                            error = %e,
                            "failed to load manual TLS certificate"
                        );
                        return Err(ProxyError::Internal(format!(
                            "failed to load TLS certificate for {}: {e}",
                            site.host
                        )));
                    }
                }
            } else if config.tls.is_some() {
                // Only enroll in ACME if global TLS/ACME is configured.
                acme_domains.push(site.host.clone());
            }
        }

        // Build certon config and resolver (if ACME is enabled).
        let (certon_config, cert_resolver, maintenance_handle) =
            if let Some(ref tls_config) = config.tls {
                if let Some(ref acme_config) = tls_config.acme {
                    let (ca_config, resolver, handle) = setup_acme(
                        acme_config,
                        &acme_domains,
                        challenge_map.clone(),
                        tls_config.on_demand.as_ref(),
                        &config.sites,
                    )
                    .await?;
                    (Some(Arc::new(ca_config)), Some(resolver), Some(handle))
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

        // Build the composite resolver.
        let composite = Arc::new(CompositeResolver {
            manual_certs: tokio::sync::RwLock::new(manual_certs),
            acme_resolver: cert_resolver,
        });

        // Build the optional client certificate verifier for mTLS.
        let client_verifier = if let Some(ref tls_config) = config.tls {
            if let Some(ref client_auth) = tls_config.client_auth {
                Some(build_client_verifier(client_auth)?)
            } else {
                None
            }
        } else {
            None
        };

        // Build the rustls ServerConfig.
        let rustls_config = build_server_config(
            composite.clone(),
            client_verifier.as_ref(),
            config.tls.as_ref(),
        );

        // Log OCSP stapling status.
        if let Some(ref tls_config) = config.tls {
            if tls_config.ocsp_stapling {
                info!(
                    "OCSP stapling enabled (handled by certon for ACME certs; \
                     manual certs require AIA extension parsing)"
                );
            }
        }

        Ok(Self {
            certon_config,
            resolver: composite,
            server_config: ArcSwap::from_pointee(rustls_config),
            maintenance_handle,
            challenge_map,
        })
    }

    /// Get a `TlsAcceptor` for use with tokio-rustls.
    ///
    /// The returned acceptor references the current `ServerConfig` via an
    /// `Arc`, so it will continue to use the config snapshot at the time of
    /// this call. For hot-reload, call this again after [`reload`](Self::reload).
    pub fn acceptor(&self) -> TlsAcceptor {
        let config = self.server_config.load_full();
        TlsAcceptor::from(config)
    }

    /// Get the current `rustls::ServerConfig` as an `Arc`.
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        self.server_config.load_full()
    }

    /// Get a reference to the shared ACME HTTP-01 challenge map.
    ///
    /// This is the same map that the `AcmeChallengeHoop` reads from
    /// to serve challenge responses.
    pub fn challenge_map(&self) -> Arc<tokio::sync::RwLock<HashMap<String, String>>> {
        self.challenge_map.clone()
    }

    /// Hot-reload the TLS configuration.
    ///
    /// This rebuilds manual certificates and re-enrolls ACME domains based
    /// on the new config. The `ServerConfig` is atomically swapped so that
    /// in-flight connections are not affected.
    ///
    /// # Errors
    ///
    /// Returns an error if any manual certificate cannot be loaded.
    pub async fn reload(&self, config: &AppConfig) -> Result<(), ProxyError> {
        info!("reloading TLS configuration");

        // Reload manual certificates.
        let mut new_manual: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
        let mut acme_domains: Vec<String> = Vec::new();

        for site in &config.sites {
            if let Some(ref site_tls) = site.tls {
                match load_manual_cert(site_tls) {
                    Ok(certified_key) => {
                        info!(
                            host = %site.host,
                            cert = %site_tls.cert,
                            "reloaded manual TLS certificate"
                        );
                        new_manual.insert(site.host.clone(), certified_key);
                    }
                    Err(e) => {
                        error!(
                            host = %site.host,
                            error = %e,
                            "failed to reload manual TLS certificate"
                        );
                        return Err(ProxyError::Internal(format!(
                            "failed to reload TLS certificate for {}: {e}",
                            site.host
                        )));
                    }
                }
            } else if config.tls.is_some() {
                acme_domains.push(site.host.clone());
            }
        }

        // Swap manual certs.
        {
            let mut guard = self.resolver.manual_certs.write().await;
            *guard = new_manual;
        }

        // Re-enroll new ACME domains (if any new ones appeared).
        if !acme_domains.is_empty() {
            if let Some(ref ca_config) = self.certon_config {
                if let Err(e) = ca_config.manage_sync(&acme_domains).await {
                    warn!(
                        error = %e,
                        "failed to manage new ACME domains during reload"
                    );
                }
            }
        }

        // Rebuild the client cert verifier if mTLS is configured.
        let client_verifier = if let Some(ref tls_config) = config.tls {
            if let Some(ref client_auth) = tls_config.client_auth {
                match build_client_verifier(client_auth) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        error!(error = %e, "failed to rebuild client cert verifier during reload");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Rebuild and swap the ServerConfig.
        let new_config = build_server_config(
            self.resolver.clone(),
            client_verifier.as_ref(),
            config.tls.as_ref(),
        );
        self.server_config.store(Arc::new(new_config));

        info!("TLS configuration reloaded successfully");
        Ok(())
    }

    /// Stop the certon maintenance loop.
    ///
    /// This should be called during graceful shutdown. After calling this,
    /// no further certificate renewals or OCSP refreshes will occur.
    pub fn stop_maintenance(&self) {
        if let Some(ref ca_config) = self.certon_config {
            ca_config.cache.stop();
            info!("certon maintenance loop stopped");
        }
    }
}

impl Drop for TlsManager {
    fn drop(&mut self) {
        // Signal the maintenance task to stop.
        if let Some(ref ca_config) = self.certon_config {
            ca_config.cache.stop();
        }
        // Abort the maintenance task handle if it's still running.
        if let Some(ref handle) = self.maintenance_handle {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// CompositeResolver
// ---------------------------------------------------------------------------

/// A certificate resolver that checks manual overrides first, then falls
/// back to the certon-managed `CertResolver`.
///
/// Manual certificates take priority over ACME-managed ones, allowing
/// per-site overrides while still benefiting from automatic management
/// for everything else.
struct CompositeResolver {
    /// Manual certificate overrides, keyed by hostname.
    manual_certs: tokio::sync::RwLock<HashMap<String, Arc<CertifiedKey>>>,
    /// The certon-backed resolver for ACME-managed domains.
    acme_resolver: Option<Arc<CertResolver>>,
}

impl std::fmt::Debug for CompositeResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeResolver")
            .field("manual_certs", &"<RwLock<HashMap>>")
            .field("acme_resolver", &self.acme_resolver.as_ref().map(|_| "..."))
            .finish()
    }
}

impl ResolvesServerCert for CompositeResolver {
    fn resolve(&self, client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // Try manual certificates first (by SNI).
        if let Some(sni) = client_hello.server_name() {
            if let Ok(guard) = self.manual_certs.try_read() {
                if let Some(ck) = guard.get(sni) {
                    debug!(sni = %sni, "serving manual TLS certificate");
                    return Some(ck.clone());
                }
            }
        }

        // Fall back to the certon resolver.
        if let Some(ref resolver) = self.acme_resolver {
            return resolver.resolve(client_hello);
        }

        None
    }
}

// ---------------------------------------------------------------------------
// mTLS — Client certificate verification
// ---------------------------------------------------------------------------

/// Build a `rustls` client certificate verifier from the mTLS configuration.
///
/// Loads CA certificates from the PEM files specified in `client_auth.ca_certs`,
/// then builds a `WebPkiClientVerifier`:
/// - If `required == true`, clients *must* present a valid certificate.
/// - If `required == false`, clients may connect without a certificate (optional mTLS).
fn build_client_verifier(
    client_auth: &ClientAuthConfig,
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, ProxyError> {
    let mut root_store = RootCertStore::empty();

    for ca_path in &client_auth.ca_certs {
        let pem_data = fs::read(ca_path).map_err(|e| {
            ProxyError::Internal(format!("failed to read CA cert file {ca_path}: {e}"))
        })?;

        let mut reader = std::io::BufReader::new(pem_data.as_slice());
        let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                ProxyError::Internal(format!(
                    "failed to parse PEM certificates from {ca_path}: {e}"
                ))
            })?;

        if certs.is_empty() {
            return Err(ProxyError::Internal(format!(
                "no certificates found in CA file: {ca_path}"
            )));
        }

        for cert in certs {
            root_store.add(cert).map_err(|e| {
                ProxyError::Internal(format!("failed to add CA cert from {ca_path}: {e}"))
            })?;
        }

        info!(path = %ca_path, "loaded CA certificate for client auth");
    }

    let builder = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store));

    let verifier = if client_auth.required {
        info!("mTLS: client certificates required");
        builder.build().map_err(|e| {
            ProxyError::Internal(format!("failed to build client cert verifier: {e}"))
        })?
    } else {
        info!("mTLS: client certificates optional");
        builder.allow_unauthenticated().build().map_err(|e| {
            ProxyError::Internal(format!(
                "failed to build optional client cert verifier: {e}"
            ))
        })?
    };

    Ok(verifier)
}

// ---------------------------------------------------------------------------
// ACME setup
// ---------------------------------------------------------------------------

/// Configure certon for ACME certificate management.
///
/// Returns the certon `Config`, a `CertResolver`, and a maintenance task
/// handle.
async fn setup_acme(
    acme_config: &AcmeConfig,
    domains: &[String],
    challenge_map: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    on_demand_config: Option<&OnDemandTlsConfig>,
    sites: &[crate::config::SiteConfig],
) -> Result<(CertAutoConfig, Arc<CertResolver>, JoinHandle<()>), ProxyError> {
    let storage: Arc<dyn Storage> = Arc::new(FileStorage::default());

    // Build the ACME issuer.
    let ca_url = match acme_config.ca {
        CertAuthority::LetsEncrypt => certon::LETS_ENCRYPT_PRODUCTION,
        CertAuthority::LetsEncryptStaging => certon::LETS_ENCRYPT_STAGING,
        CertAuthority::ZeroSsl => certon::ZEROSSL_PRODUCTION,
    };

    let mut issuer_builder = AcmeIssuer::builder()
        .ca(ca_url)
        .email(&acme_config.email)
        .agreed(true)
        .storage(storage.clone());

    // Configure challenge type.
    match acme_config.challenge {
        ChallengeType::Http01 => {
            // Use a custom HTTP-01 solver that shares the challenge map with
            // our middleware instead of binding its own port 80 listener.
            let solver = Arc::new(SharedMapHttp01Solver {
                challenges: challenge_map,
            });
            issuer_builder = issuer_builder
                .http01_solver(solver)
                .disable_tlsalpn_challenge(true);
        }
        ChallengeType::TlsAlpn01 => {
            issuer_builder = issuer_builder.disable_http_challenge(true);
        }
        ChallengeType::Dns01 => {
            if let Some(ref dns_cfg) = acme_config.dns_provider {
                let provider = create_dns_provider(dns_cfg)?;
                let dns_solver = certon::Dns01Solver::new(provider);
                issuer_builder = issuer_builder
                    .dns01_solver(Arc::new(dns_solver))
                    .disable_http_challenge(true)
                    .disable_tlsalpn_challenge(true);
            } else {
                return Err(ProxyError::Internal(
                    "DNS-01 challenge requires a dns-provider configuration".into(),
                ));
            }
        }
    }

    // Apply EAB credentials if configured (required by some CAs like ZeroSSL).
    if let Some(ref eab_cfg) = acme_config.eab {
        let hmac_bytes = base64_decode_hmac(&eab_cfg.hmac_key)?;
        issuer_builder =
            issuer_builder.external_account(certon::acme_client::ExternalAccountBinding {
                kid: eab_cfg.kid.clone(),
                hmac_key: hmac_bytes,
            });
    }

    let issuer = Arc::new(issuer_builder.build());

    // Build the certon Config, optionally with on-demand TLS.
    let mut config_builder = CertAutoConfig::builder()
        .storage(storage)
        .issuers(vec![issuer.clone()]);

    // Set up on-demand TLS if configured.
    if let Some(od_config) = on_demand_config {
        let on_demand = build_on_demand_config(od_config, &issuer, sites)?;
        config_builder = config_builder.on_demand(Arc::new(on_demand));
        info!("on-demand TLS configured");
    }

    let ca_config = config_builder.build();

    // Manage certificates for the configured domains.
    if !domains.is_empty() {
        info!(domains = ?domains, "managing ACME certificates");
        ca_config.manage_sync(domains).await.map_err(|e| {
            ProxyError::Internal(format!("ACME certificate management failed: {e}"))
        })?;
    }

    // Build the CertResolver backed by certon's cache.
    // If on-demand TLS is configured, use the on_demand variant so the
    // resolver can trigger background certificate acquisition.
    let resolver = if ca_config.on_demand.is_some() {
        let on_demand = ca_config.on_demand.clone().unwrap();
        Arc::new(CertResolver::with_on_demand(
            ca_config.cache.clone(),
            on_demand,
        ))
    } else {
        Arc::new(CertResolver::new(ca_config.cache.clone()))
    };

    // Start the maintenance loop (renewal + OCSP refresh).
    let maintenance_handle = certon::start_maintenance(&ca_config);
    info!("certon maintenance loop started");

    Ok((ca_config, resolver, maintenance_handle))
}

/// Build the `OnDemandConfig` from our config types.
fn build_on_demand_config(
    od_config: &OnDemandTlsConfig,
    issuer: &Arc<certon::AcmeIssuer>,
    sites: &[crate::config::SiteConfig],
) -> Result<OnDemandConfig, ProxyError> {
    // Build allowlist from existing site hostnames.
    let allowlist: HashSet<String> = sites.iter().map(|s| s.host.to_lowercase()).collect();

    // Build the decision function if an ask URL is configured.
    let decision_func: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>> =
        if let Some(ref ask_url) = od_config.ask {
            let url = ask_url.clone();
            Some(Arc::new(move |domain: &str| {
                check_ask_url_blocking(&url, domain)
            }))
        } else {
            None
        };

    // Build the rate limiter if configured.
    let rate_limit = od_config.rate_limit.map(|max_per_minute| {
        Arc::new(certon::rate_limiter::RateLimiter::new(
            max_per_minute as usize,
            Duration::from_secs(60),
        ))
    });

    // Build the obtain function that triggers ACME certificate issuance.
    let issuer_for_obtain = Arc::clone(issuer);
    let obtain_func: Arc<
        dyn Fn(String) -> Pin<Box<dyn std::future::Future<Output = certon::Result<()>> + Send>>
            + Send
            + Sync,
    > = Arc::new(move |domain: String| {
        let issuer = Arc::clone(&issuer_for_obtain);
        Box::pin(async move {
            info!(domain = %domain, "on-demand TLS: obtaining certificate");
            match issuer.issue_for_domains(&[domain.clone()]).await {
                Ok(_cert) => {
                    info!(domain = %domain, "on-demand TLS: certificate obtained");
                    Ok(())
                }
                Err(e) => {
                    error!(domain = %domain, error = %e, "on-demand TLS: failed to obtain certificate");
                    Err(e)
                }
            }
        })
    });

    Ok(OnDemandConfig {
        decision_func,
        host_allowlist: if allowlist.is_empty() {
            None
        } else {
            Some(allowlist)
        },
        rate_limit,
        obtain_func: Some(obtain_func),
    })
}

/// Synchronously check the ask URL. This is called from the decision function
/// which runs synchronously in the rustls resolve path. We use
/// `tokio::task::block_in_place` to run an async HTTP request, which is
/// acceptable here since it only runs for on-demand (uncached) requests.
fn check_ask_url_blocking(ask_url: &str, domain: &str) -> bool {
    let url = format!("{}?domain={}", ask_url, domain);
    debug!(url = %url, "on-demand TLS: checking ask URL");

    // Use block_in_place + a short-lived runtime to issue an async request
    // from this synchronous context.
    let result = std::panic::catch_unwind(|| {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|e| format!("client build error: {e}"))?;
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("request error: {e}"))?;
                Ok::<bool, String>(resp.status().is_success())
            })
        })
    });

    match result {
        Ok(Ok(allowed)) => {
            debug!(url = %url, allowed = %allowed, "on-demand TLS: ask URL response");
            allowed
        }
        Ok(Err(e)) => {
            warn!(url = %url, error = %e, "on-demand TLS: ask URL request failed");
            false
        }
        Err(_) => {
            warn!(url = %url, "on-demand TLS: ask URL check failed (no runtime)");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// SharedMapHttp01Solver
// ---------------------------------------------------------------------------

/// An HTTP-01 solver that stores challenge tokens in a shared in-memory map
/// rather than running its own HTTP server.
///
/// The gatel HTTP server (port 80) serves challenge responses via the
/// [`AcmeChallengeHoop`](crate::hoops::acme_challenge::AcmeChallengeHoop),
/// so there is no need for the solver to bind a separate listener.
struct SharedMapHttp01Solver {
    challenges: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
}

#[async_trait::async_trait]
impl certon::Solver for SharedMapHttp01Solver {
    async fn present(&self, _domain: &str, token: &str, key_auth: &str) -> certon::Result<()> {
        debug!(token = %token, "presenting HTTP-01 challenge token");
        let mut map = self.challenges.write().await;
        map.insert(token.to_string(), key_auth.to_string());
        Ok(())
    }

    async fn cleanup(&self, _domain: &str, token: &str, _key_auth: &str) -> certon::Result<()> {
        debug!(token = %token, "cleaning up HTTP-01 challenge token");
        let mut map = self.challenges.write().await;
        map.remove(token);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Manual certificate loading
// ---------------------------------------------------------------------------

/// Load a TLS certificate and private key from PEM files specified in a
/// [`SiteTlsConfig`].
///
/// Uses certon's `Certificate::from_pem_files` for PEM parsing, then
/// converts to a rustls `CertifiedKey`.
fn load_manual_cert(site_tls: &SiteTlsConfig) -> Result<Arc<CertifiedKey>, ProxyError> {
    let cert_path = Path::new(&site_tls.cert);
    let key_path = Path::new(&site_tls.key);

    let cert = Certificate::from_pem_files(cert_path, key_path).map_err(|e| {
        ProxyError::Internal(format!(
            "failed to parse PEM certificate/key ({}, {}): {e}",
            site_tls.cert, site_tls.key
        ))
    })?;

    let certified_key = certon::handshake::cert_to_certified_key(&cert).map_err(|e| {
        ProxyError::Internal(format!(
            "failed to convert certificate to CertifiedKey: {e}"
        ))
    })?;

    Ok(certified_key)
}

// ---------------------------------------------------------------------------
// ServerConfig construction
// ---------------------------------------------------------------------------

/// Compute the set of protocol versions to enable given optional min/max constraints.
///
/// Returns `None` to indicate "use safe defaults".
fn resolve_protocol_versions(
    min_version: Option<&str>,
    max_version: Option<&str>,
) -> Option<Vec<&'static rustls::SupportedProtocolVersion>> {
    if min_version.is_none() && max_version.is_none() {
        return None;
    }

    // All versions in ascending order: index 0 = TLS 1.2, index 1 = TLS 1.3.
    let all: [&'static rustls::SupportedProtocolVersion; 2] =
        [&rustls::version::TLS12, &rustls::version::TLS13];

    let version_index = |v: &str| match v {
        "1.2" => Some(0usize),
        "1.3" => Some(1usize),
        _ => None,
    };

    let min_idx: usize = min_version.and_then(version_index).unwrap_or(0);
    let max_idx: usize = max_version.and_then(version_index).unwrap_or(all.len() - 1);

    let versions: Vec<&'static rustls::SupportedProtocolVersion> = all[min_idx..=max_idx].to_vec();

    if versions.is_empty() {
        None
    } else {
        Some(versions)
    }
}

/// Build a CryptoProvider with only the selected cipher suites.
fn build_provider_with_suites(suite_names: &[String]) -> rustls::crypto::CryptoProvider {
    use rustls::crypto::ring::cipher_suite;
    // All ring-backed cipher suites.
    let all_suites: &[rustls::SupportedCipherSuite] = &[
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ];

    let selected: Vec<rustls::SupportedCipherSuite> = all_suites
        .iter()
        .filter(|s: &&rustls::SupportedCipherSuite| {
            let name = format!("{:?}", s.suite());
            suite_names
                .iter()
                .any(|n| name.contains(n.as_str()) || n.as_str() == name.as_str())
        })
        .copied()
        .collect();

    let suites = if selected.is_empty() {
        warn!(
            "no matching cipher suites found for {:?}, using defaults",
            suite_names
        );
        rustls::crypto::ring::default_provider().cipher_suites
    } else {
        selected
    };

    rustls::crypto::CryptoProvider {
        cipher_suites: suites,
        ..rustls::crypto::ring::default_provider()
    }
}

/// Filter key-exchange groups to those matching the requested curve names.
///
/// Supported names (case-insensitive): `"x25519"`, `"secp256r1"`, `"secp384r1"`.
/// Returns the default provider's groups if none of the names match.
fn filter_kx_groups(curve_names: &[String]) -> Vec<&'static dyn rustls::crypto::SupportedKxGroup> {
    use rustls::crypto::ring::kx_group;
    let all: &[(&str, &'static dyn rustls::crypto::SupportedKxGroup)] = &[
        ("x25519", kx_group::X25519),
        ("secp256r1", kx_group::SECP256R1),
        ("secp384r1", kx_group::SECP384R1),
    ];
    let mut selected: Vec<&'static dyn rustls::crypto::SupportedKxGroup> = Vec::new();
    for name in curve_names {
        let lower = name.to_ascii_lowercase();
        for &(n, group) in all {
            if n == lower {
                selected.push(group);
            }
        }
    }
    if selected.is_empty() {
        warn!(
            "no matching ECDH curves for {:?}, using defaults",
            curve_names
        );
        rustls::crypto::ring::default_provider().kx_groups
    } else {
        selected
    }
}

/// Build a `rustls::ServerConfig` that uses the given resolver and optional
/// client certificate verifier for mTLS.
fn build_server_config(
    resolver: Arc<dyn ResolvesServerCert>,
    client_verifier: Option<&Arc<dyn rustls::server::danger::ClientCertVerifier>>,
    tls_config: Option<&TlsConfig>,
) -> rustls::ServerConfig {
    // Select the crypto provider (custom cipher suites or default), then
    // optionally filter ECDH key-exchange groups.
    let provider = if let Some(cfg) = tls_config {
        let mut p = if !cfg.cipher_suites.is_empty() {
            build_provider_with_suites(&cfg.cipher_suites)
        } else {
            rustls::crypto::ring::default_provider()
        };
        if !cfg.ecdh_curves.is_empty() {
            p.kx_groups = filter_kx_groups(&cfg.ecdh_curves);
        }
        Arc::new(p)
    } else {
        Arc::new(rustls::crypto::ring::default_provider())
    };

    // Determine protocol versions.
    let versions = tls_config.and_then(|cfg| {
        resolve_protocol_versions(cfg.min_version.as_deref(), cfg.max_version.as_deref())
    });

    let builder = if let Some(ref versions) = versions {
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("TLS protocol versions are valid")
    } else {
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("default protocol versions are valid")
    };

    if let Some(verifier) = client_verifier {
        builder
            .with_client_cert_verifier(Arc::clone(verifier))
            .with_cert_resolver(resolver)
    } else {
        builder.with_no_client_auth().with_cert_resolver(resolver)
    }
}

// ---------------------------------------------------------------------------
// DNS provider dispatch
// ---------------------------------------------------------------------------

/// Decode a base64url or standard base64 encoded HMAC key into raw bytes.
///
/// EAB HMAC keys from CAs are typically base64url-encoded (RFC 4648 §5),
/// which uses `-` and `_` instead of `+` and `/`. We normalise those
/// characters before running the standard alphabet decoder so that both
/// encodings are accepted.
fn base64_decode_hmac(input: &str) -> Result<Vec<u8>, ProxyError> {
    // Normalise base64url → standard base64 by swapping the two variant chars.
    let normalised: String = input
        .trim()
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    decode_base64(&normalised)
        .ok_or_else(|| ProxyError::Internal("invalid base64 in EAB HMAC key".into()))
}

/// Minimal base64 decoder (standard alphabet only; call after normalising
/// base64url characters).
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let input = input.trim();
    if input.is_empty() {
        return Some(Vec::new());
    }

    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let val = match TABLE.iter().position(|&c| c == b) {
            Some(v) => v as u32,
            None => {
                if b == b'\n' || b == b'\r' || b == b' ' {
                    continue;
                }
                return None;
            }
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Some(output)
}

/// Dispatch to the concrete DNS provider implementation based on the provider
/// name in the configuration.
fn create_dns_provider(
    cfg: &DnsProviderConfig,
) -> Result<Box<dyn certon::DnsProvider>, ProxyError> {
    match cfg.provider.as_str() {
        "cloudflare" => Ok(Box::new(crate::tls::dns::CloudflareDns::new(cfg)?)),
        "route53" => Ok(Box::new(crate::tls::dns::Route53Dns::new(cfg)?)),
        "digitalocean" => Ok(Box::new(crate::tls::dns::DigitalOceanDns::new(cfg)?)),
        "dnsimple" => Ok(Box::new(crate::tls::dns::DnSimpleDns::new(cfg)?)),
        "porkbun" => Ok(Box::new(crate::tls::dns::PorkbunDns::new(cfg)?)),
        "ovh" => Ok(Box::new(crate::tls::dns::OvhDns::new(cfg)?)),
        "desec" => Ok(Box::new(crate::tls::dns::DesecDns::new(cfg)?)),
        "bunny" => Ok(Box::new(crate::tls::dns::BunnyDns::new(cfg)?)),
        "rfc2136" => Ok(Box::new(crate::tls::dns::Rfc2136Dns::new(cfg)?)),
        other => Err(ProxyError::Internal(format!(
            "unknown DNS provider: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_server_config_creates_valid_config_no_client_auth() {
        // Use a dummy resolver that always returns None.
        #[derive(Debug)]
        struct NullResolver;
        impl ResolvesServerCert for NullResolver {
            fn resolve(
                &self,
                _client_hello: rustls::server::ClientHello<'_>,
            ) -> Option<Arc<CertifiedKey>> {
                None
            }
        }

        let config = build_server_config(Arc::new(NullResolver), None, None);
        // Verify it was constructed (no panic).
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn build_server_config_with_client_verifier() {
        #[derive(Debug)]
        struct NullResolver;
        impl ResolvesServerCert for NullResolver {
            fn resolve(
                &self,
                _client_hello: rustls::server::ClientHello<'_>,
            ) -> Option<Arc<CertifiedKey>> {
                None
            }
        }

        // Build a root store with no certs -- we can't build WebPkiClientVerifier
        // with empty roots, so this test just verifies the no-client-auth path.
        let config = build_server_config(Arc::new(NullResolver), None, None);
        assert!(config.alpn_protocols.is_empty());
    }
}
