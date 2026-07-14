//! Local CA — Caddy-style `tls internal` for gatel.
//!
//! A long-lived self-signed root CA + short-lived intermediate sign leaf
//! certificates on demand at TLS handshake time. The root certificate is
//! persisted to disk in the gatel data directory; the user installs it into
//! the OS trust store with the `gatel trust` subcommand so that local browsers
//! accept the issued leaves without warnings.
//!
//! Lifetimes mirror Caddy's defaults: root ~10 years, intermediate 7 days,
//! leaf 12 hours. Leaves are cached in memory keyed by SNI hostname and
//! re-issued before expiry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::string::Ia5String;
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Root CA validity: 10 years.
const ROOT_LIFETIME_DAYS: i64 = 365 * 10;
/// Intermediate CA validity: 7 days.
const INTERMEDIATE_LIFETIME_DAYS: i64 = 7;
/// Leaf certificate validity: 12 hours.
const LEAF_LIFETIME_HOURS: i64 = 12;
/// Re-issue a leaf when it has less than this fraction of its lifetime left.
const LEAF_RENEW_RATIO: f64 = 0.2;

/// Filenames inside the local CA storage directory.
const ROOT_CERT_FILE: &str = "root.pem";
const ROOT_KEY_FILE: &str = "root.key";
const INT_CERT_FILE: &str = "intermediate.pem";
const INT_KEY_FILE: &str = "intermediate.key";

/// Default subject CN for the root CA.
const DEFAULT_ROOT_CN: &str = "Gatel Local Authority - Root";
/// Default subject CN for the intermediate.
const DEFAULT_INT_CN: &str = "Gatel Local Authority - Intermediate";

/// On-disk + in-memory local certificate authority.
///
/// Use [`LocalCa::load_or_create`] to bootstrap a CA rooted at the gatel data
/// directory (or any other path). The CA owns its intermediate signing key and
/// can issue leaf certificates for arbitrary hostnames via [`LocalCa::issue`].
pub struct LocalCa {
    storage_dir: PathBuf,
    root_cert_der: CertificateDer<'static>,
    root_cert_pem: String,
    intermediate_issuer: Issuer<'static, KeyPair>,
    intermediate_chain_der: Vec<CertificateDer<'static>>,
    cache: RwLock<HashMap<String, CachedLeaf>>,
}

#[derive(Clone)]
struct CachedLeaf {
    certified_key: Arc<CertifiedKey>,
    not_after: OffsetDateTime,
}

impl LocalCa {
    /// Load the root + intermediate from `storage_dir`, generating fresh ones
    /// if any expected file is missing or unreadable.
    pub fn load_or_create(storage_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let storage_dir = storage_dir.into();
        std::fs::create_dir_all(&storage_dir)?;

        let (root_cert_pem, root_key) = load_or_generate_root(&storage_dir)?;
        let (intermediate_cert_pem, intermediate_key) =
            load_or_generate_intermediate(&storage_dir, &root_cert_pem, &root_key)?;

        let root_cert_der = pem_to_der(&root_cert_pem).map_err(map_pem_err)?;
        let intermediate_cert_der = pem_to_der(&intermediate_cert_pem).map_err(map_pem_err)?;

        // Build the issuer that signs leaves. We do this from the intermediate
        // cert + key so a regenerated intermediate (fresh `LocalCa`) picks up
        // automatically.
        let intermediate_issuer =
            Issuer::from_ca_cert_pem(&intermediate_cert_pem, intermediate_key)
                .map_err(map_rcgen_err)?;

        // Leaf chain served to clients is [intermediate]. The root is *not*
        // included — clients trust it directly from their trust store.
        let intermediate_chain_der = vec![intermediate_cert_der];

        info!(
            storage_dir = %storage_dir.display(),
            "local CA ready"
        );

        Ok(Self {
            storage_dir,
            root_cert_der,
            root_cert_pem,
            intermediate_issuer,
            intermediate_chain_der,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// Path to the root CA PEM file on disk.
    pub fn root_cert_path(&self) -> PathBuf {
        self.storage_dir.join(ROOT_CERT_FILE)
    }

    /// Root certificate as PEM (the bytes a user installs into their trust store).
    pub fn root_cert_pem(&self) -> &str {
        &self.root_cert_pem
    }

    /// Root certificate as DER (for direct OS trust-store APIs).
    pub fn root_cert_der(&self) -> &CertificateDer<'static> {
        &self.root_cert_der
    }

    /// The configured storage directory.
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// Look up (or issue) a leaf certificate for the given hostname. The
    /// returned `CertifiedKey` is suitable for use from a rustls resolver.
    pub async fn certificate_for(&self, hostname: &str) -> Result<Arc<CertifiedKey>, LocalCaError> {
        let normalized = hostname.to_ascii_lowercase();

        if let Some(entry) = self.cache.read().await.get(&normalized).cloned()
            && !leaf_needs_renew(&entry)
        {
            return Ok(entry.certified_key);
        }

        let mut cache = self.cache.write().await;
        // Double-check after acquiring the write lock.
        if let Some(entry) = cache.get(&normalized).cloned()
            && !leaf_needs_renew(&entry)
        {
            return Ok(entry.certified_key);
        }

        debug!(hostname = %normalized, "issuing local CA leaf certificate");
        let (cert, leaf_key, not_after) = self.issue_leaf(&normalized)?;
        let leaf_der = cert.der().clone();

        // Build the served chain: [leaf, intermediate]
        let mut chain: Vec<CertificateDer<'static>> = Vec::with_capacity(2);
        chain.push(leaf_der);
        chain.extend(self.intermediate_chain_der.iter().cloned());

        let pkcs8 = PrivatePkcs8KeyDer::from(leaf_key.serialize_der());
        let signing_key =
            rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(pkcs8))
                .map_err(|e| LocalCaError::Sign(e.to_string()))?;
        let certified_key = Arc::new(CertifiedKey::new(chain, signing_key));

        cache.insert(
            normalized.clone(),
            CachedLeaf {
                certified_key: certified_key.clone(),
                not_after,
            },
        );

        Ok(certified_key)
    }

    /// Issue a new leaf certificate for the given hostname, signed by the
    /// intermediate. Both DNS SANs and IP-literal SANs are honoured.
    fn issue_leaf(
        &self,
        hostname: &str,
    ) -> Result<(rcgen::Certificate, KeyPair, OffsetDateTime), LocalCaError> {
        let leaf_key =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(map_local_rcgen_err)?;
        let mut params =
            CertificateParams::new(Vec::<String>::new()).map_err(map_local_rcgen_err)?;

        let san = match hostname.parse::<std::net::IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => {
                SanType::DnsName(Ia5String::try_from(hostname.to_string()).map_err(|e| {
                    LocalCaError::Generate(format!("invalid SNI '{hostname}': {e}"))
                })?)
            }
        };
        params.subject_alt_names = vec![san];
        params.distinguished_name.push(DnType::CommonName, hostname);

        let now = OffsetDateTime::now_utc();
        let not_before = now - TimeDuration::minutes(1);
        let not_after = now + TimeDuration::hours(LEAF_LIFETIME_HOURS);
        params.not_before = not_before;
        params.not_after = not_after;
        params.is_ca = IsCa::ExplicitNoCa;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let cert = params
            .signed_by(&leaf_key, &self.intermediate_issuer)
            .map_err(map_local_rcgen_err)?;
        Ok((cert, leaf_key, not_after))
    }
}

fn leaf_needs_renew(entry: &CachedLeaf) -> bool {
    let total = TimeDuration::hours(LEAF_LIFETIME_HOURS).as_seconds_f64();
    let remaining = (entry.not_after - OffsetDateTime::now_utc()).as_seconds_f64();
    remaining <= total * LEAF_RENEW_RATIO
}

fn load_or_generate_root(dir: &Path) -> std::io::Result<(String, KeyPair)> {
    let cert_path = dir.join(ROOT_CERT_FILE);
    let key_path = dir.join(ROOT_KEY_FILE);

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        match KeyPair::from_pem(&key_pem) {
            Ok(key) => return Ok((cert_pem, key)),
            Err(e) => warn!(
                error = %e,
                path = %key_path.display(),
                "failed to load existing local CA root key; regenerating"
            ),
        }
    }

    info!(dir = %dir.display(), "generating new local CA root");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| std::io::Error::other(format!("failed to generate local CA root key: {e}")))?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(map_rcgen_err)?;
    params
        .distinguished_name
        .push(DnType::CommonName, DEFAULT_ROOT_CN);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Gatel");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - TimeDuration::minutes(1);
    params.not_after = now + TimeDuration::days(ROOT_LIFETIME_DAYS);
    let cert = params.self_signed(&key).map_err(map_rcgen_err)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();

    write_secret(&cert_path, cert_pem.as_bytes())?;
    write_secret(&key_path, key_pem.as_bytes())?;

    Ok((cert_pem, key))
}

fn load_or_generate_intermediate(
    dir: &Path,
    root_cert_pem: &str,
    root_key: &KeyPair,
) -> std::io::Result<(String, KeyPair)> {
    let cert_path = dir.join(INT_CERT_FILE);
    let key_path = dir.join(INT_KEY_FILE);

    if cert_path.exists()
        && key_path.exists()
        && let Ok(cert_pem) = std::fs::read_to_string(&cert_path)
        && let Ok(key_pem) = std::fs::read_to_string(&key_path)
        && let Ok(key) = KeyPair::from_pem(&key_pem)
        && !intermediate_needs_rotation(&cert_path)
    {
        return Ok((cert_pem, key));
    }

    info!(dir = %dir.display(), "issuing new local CA intermediate");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| std::io::Error::other(format!("failed to generate intermediate key: {e}")))?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(map_rcgen_err)?;
    params
        .distinguished_name
        .push(DnType::CommonName, DEFAULT_INT_CN);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Gatel");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - TimeDuration::minutes(1);
    params.not_after = now + TimeDuration::days(INTERMEDIATE_LIFETIME_DAYS);

    let root_issuer = Issuer::from_ca_cert_pem(root_cert_pem, root_key.clone_for_signing()?)
        .map_err(map_rcgen_err)?;
    let cert = params
        .signed_by(&key, &root_issuer)
        .map_err(map_rcgen_err)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();

    write_secret(&cert_path, cert_pem.as_bytes())?;
    write_secret(&key_path, key_pem.as_bytes())?;
    Ok((cert_pem, key))
}

/// Re-issue the intermediate when its file is older than 80% of its
/// configured lifetime — a coarse proxy for "less than 20% of validity left"
/// without pulling in a full X.509 parser. The intermediate's lifetime is a
/// fixed constant in this module, so the file's `mtime` is a reliable signal.
fn intermediate_needs_rotation(cert_path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(cert_path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    let lifetime = std::time::Duration::from_secs(INTERMEDIATE_LIFETIME_DAYS as u64 * 24 * 3600);
    age >= lifetime * 4 / 5
}

/// Convenience trait extension: rcgen's `KeyPair` does not implement `Clone`
/// directly, but a private key can be cloned by round-tripping through PEM.
trait KeyPairExt {
    fn clone_for_signing(&self) -> std::io::Result<KeyPair>;
}

impl KeyPairExt for KeyPair {
    fn clone_for_signing(&self) -> std::io::Result<KeyPair> {
        KeyPair::from_pem(&self.serialize_pem())
            .map_err(|e| std::io::Error::other(format!("failed to re-load key for signing: {e}")))
    }
}

fn pem_to_der(pem: &str) -> Result<CertificateDer<'static>, String> {
    let mut iter = CertificateDer::pem_reader_iter(pem.as_bytes());
    match iter.next() {
        Some(Ok(der)) => Ok(der),
        Some(Err(e)) => Err(format!("PEM parse error: {e}")),
        None => Err("no certificate found in PEM".into()),
    }
}

/// Write a secret file with `0o600` permissions on Unix; default on Windows.
fn write_secret(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Resolve the default storage directory for the local CA.
///
/// On all platforms this is `<data-dir>/gatel/pki/authorities/local`, where
/// `<data-dir>` is the platform-specific user data directory (Linux:
/// `~/.local/share`, macOS: `~/Library/Application Support`, Windows:
/// `%LOCALAPPDATA%`). Falls back to `./gatel-pki` if no data dir is available.
pub fn default_storage_dir() -> PathBuf {
    if let Some(d) = dirs::data_local_dir() {
        return d
            .join("gatel")
            .join("pki")
            .join("authorities")
            .join("local");
    }
    PathBuf::from("gatel-pki").join("authorities").join("local")
}

#[derive(Debug, thiserror::Error)]
pub enum LocalCaError {
    #[error("failed to generate certificate: {0}")]
    Generate(String),

    #[error("failed to build signing key: {0}")]
    Sign(String),
}

fn map_rcgen_err(e: rcgen::Error) -> std::io::Error {
    std::io::Error::other(format!("rcgen error: {e}"))
}

fn map_pem_err(e: String) -> std::io::Error {
    std::io::Error::other(e)
}

fn map_local_rcgen_err(e: rcgen::Error) -> LocalCaError {
    LocalCaError::Generate(e.to_string())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn issues_and_caches_leaf() {
        let dir = tempdir().unwrap();
        let ca = LocalCa::load_or_create(dir.path()).unwrap();

        let cert1 = ca.certificate_for("localhost").await.unwrap();
        let cert2 = ca.certificate_for("localhost").await.unwrap();
        assert!(Arc::ptr_eq(&cert1, &cert2), "expected cached cert");

        let cert3 = ca.certificate_for("other.test").await.unwrap();
        assert!(!Arc::ptr_eq(&cert1, &cert3));
    }

    #[tokio::test]
    async fn root_persists_across_reloads() {
        let dir = tempdir().unwrap();
        let root_pem_first;
        {
            let ca = LocalCa::load_or_create(dir.path()).unwrap();
            root_pem_first = ca.root_cert_pem().to_string();
        }
        let ca = LocalCa::load_or_create(dir.path()).unwrap();
        assert_eq!(ca.root_cert_pem(), root_pem_first);
    }

    #[tokio::test]
    async fn issues_for_ip_literal() {
        let dir = tempdir().unwrap();
        let ca = LocalCa::load_or_create(dir.path()).unwrap();
        let cert = ca.certificate_for("127.0.0.1").await.unwrap();
        assert!(!cert.cert.is_empty());
    }
}
