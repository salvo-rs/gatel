//! The development certificate authority.
//!
//! `tls internal` exists so that "test it over HTTPS" does not mean either
//! disabling verification or running a public issuance flow against a machine
//! that has no public name. The root is generated once, stays on this machine,
//! and is installed into the OS trust store deliberately with `gatel trust`.
//!
//! Its leaves are short-lived on purpose: one that escapes this machine stops
//! working within days.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use time::OffsetDateTime;

use crate::certificate::Certificate;

/// How long the root lasts. Long, because reinstalling it into a trust store is
/// a manual step.
const ROOT_DAYS: i64 = 3650;
/// How long a leaf lasts. Short, because nothing should come to depend on one.
const LEAF_DAYS: i64 = 14;

/// A locally generated authority.
pub struct InternalCa {
    root_pem: String,
    key_pem: String,
    issuer: Issuer<'static, KeyPair>,
    directory: PathBuf,
}

impl std::fmt::Debug for InternalCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalCa")
            .field("directory", &self.directory)
            .finish()
    }
}

impl InternalCa {
    /// Load the authority from `directory`, generating it the first time.
    ///
    /// Generating rather than failing is deliberate: the first `gatel run` with
    /// `tls internal` should work, and the operator is then told to trust it.
    pub fn load_or_create(directory: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("cannot create `{}`: {error}", directory.display()))?;
        let root_path = directory.join("root.pem");
        let key_path = directory.join("root-key.pem");

        let (root_pem, key_pem) = match (
            std::fs::read_to_string(&root_path),
            std::fs::read_to_string(&key_path),
        ) {
            (Ok(root), Ok(key)) => (root, key),
            _ => {
                let generated = generate_root()?;
                write_private(&key_path, &generated.1)?;
                std::fs::write(&root_path, &generated.0)
                    .map_err(|error| format!("cannot write the root certificate: {error}"))?;
                tracing::info!(
                    target: "gatel::certificate",
                    event = "internal_ca_created",
                    directory = %directory.display(),
                    "run `gatel trust` to make this machine accept it",
                );
                generated
            }
        };

        let key =
            KeyPair::from_pem(&key_pem).map_err(|error| format!("the root key is unusable: {error}"))?;
        // The issuer is rebuilt from the same parameters the root was signed
        // with, rather than parsed back out of the PEM: Gatel generated both
        // halves, so re-deriving is exact and needs no X.509 reader.
        let issuer = Issuer::new(root_params(), key);

        Ok(Self {
            root_pem,
            key_pem,
            issuer,
            directory: directory.to_path_buf(),
        })
    }

    /// Sign a certificate for one host.
    pub fn issue(&self, host: &str) -> Result<Certificate, String> {
        let mut params = CertificateParams::new(vec![host.to_string()])
            .map_err(|error| format!("`{host}` cannot appear in a certificate: {error}"))?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host);
        params.not_before = OffsetDateTime::now_utc();
        params.not_after = OffsetDateTime::now_utc() + time::Duration::days(LEAF_DAYS);
        // A browser reaching a gateway by address needs an IP SAN; a DNS SAN
        // with the same text does not satisfy it.
        if let Ok(address) = host.parse::<std::net::IpAddr>() {
            params.subject_alt_names = vec![SanType::IpAddress(address)];
        }

        let key = KeyPair::generate().map_err(|error| format!("cannot generate a key: {error}"))?;
        let certificate = params
            .signed_by(&key, &self.issuer)
            .map_err(|error| format!("cannot sign a certificate for `{host}`: {error}"))?;

        Certificate::from_pem(
            certificate.pem().as_bytes(),
            key.serialize_pem().as_bytes(),
            &format!("internal:{host}"),
        )
    }

    /// The root, in PEM, for installing into a trust store.
    pub fn root_pem(&self) -> &str {
        &self.root_pem
    }

    pub fn root_path(&self) -> PathBuf {
        self.directory.join("root.pem")
    }

    /// Kept so a caller can back the authority up. Never logged.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Where the authority lives by default: a per-user data directory rather
    /// than a shared one, because it is a development tool and not a system
    /// trust anchor.
    pub fn default_directory() -> PathBuf {
        let base = std::env::var_os("GATEL_DATA_DIR").map(PathBuf::from).or_else(|| {
            if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
            } else {
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
            }
        });
        base.unwrap_or_else(std::env::temp_dir)
            .join("gatel")
            .join("internal-ca")
    }
}

/// The root's parameters, in one place so signing it and later issuing from it
/// cannot drift apart.
fn root_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    // Constrained(0): this authority may sign leaves, never another authority.
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "Gatel development CA");
    params.distinguished_name.push(DnType::OrganizationName, "Gatel");
    params
}

fn generate_root() -> Result<(String, String), String> {
    let mut params = root_params();
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + time::Duration::days(ROOT_DAYS);

    let key = KeyPair::generate().map_err(|error| format!("cannot generate a root key: {error}"))?;
    let certificate = params
        .self_signed(&key)
        .map_err(|error| format!("cannot sign the root: {error}"))?;
    Ok((certificate.pem(), key.serialize_pem()))
}

/// Write a private key so only its owner can read it.
fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("cannot write `{}`: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot restrict `{}`: {error}", path.display()))?;
    }
    Ok(())
}

/// How long a freshly issued leaf is valid, so a caller can plan renewal.
pub const fn leaf_lifetime() -> Duration {
    Duration::from_secs(LEAF_DAYS as u64 * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gatel-ca-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_authority_is_generated_once_and_then_reused() {
        let directory = TempDir::new("reuse");
        let first = InternalCa::load_or_create(&directory.0).unwrap();
        let second = InternalCa::load_or_create(&directory.0).unwrap();
        assert_eq!(
            first.root_pem(),
            second.root_pem(),
            "regenerating would invalidate whatever the operator already trusted"
        );
    }

    #[test]
    fn it_issues_a_usable_certificate_for_a_host() {
        let directory = TempDir::new("issue");
        let authority = InternalCa::load_or_create(&directory.0).unwrap();
        let certificate = authority.issue("app.localhost").unwrap();
        assert_eq!(certificate.certified.cert.len(), 1);
        assert_eq!(certificate.source, "internal:app.localhost");
    }

    #[test]
    fn it_issues_for_an_address_as_well_as_a_name() {
        let directory = TempDir::new("ip");
        let authority = InternalCa::load_or_create(&directory.0).unwrap();
        assert!(authority.issue("127.0.0.1").is_ok());
    }

    #[test]
    fn two_certificates_for_one_host_have_different_keys() {
        let directory = TempDir::new("distinct");
        let authority = InternalCa::load_or_create(&directory.0).unwrap();
        let first = authority.issue("app.localhost").unwrap();
        let second = authority.issue("app.localhost").unwrap();
        assert_ne!(first.certified.cert[0], second.certified.cert[0]);
    }

    #[test]
    fn the_root_is_written_where_trust_can_find_it() {
        let directory = TempDir::new("path");
        let authority = InternalCa::load_or_create(&directory.0).unwrap();
        assert!(authority.root_path().exists());
        assert!(authority.root_pem().contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn a_leaf_expires_well_before_the_root() {
        assert!(leaf_lifetime() < Duration::from_secs(ROOT_DAYS as u64 * 86_400));
        assert!(leaf_lifetime() <= Duration::from_secs(30 * 86_400));
    }
}
