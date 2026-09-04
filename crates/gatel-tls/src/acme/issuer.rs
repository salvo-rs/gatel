//! Ordering one certificate.
//!
//! Gatel generates the key and the request; `certon` does the account, the
//! order, the challenge exchange and the download. Keeping the key on this side
//! means the private half of a certificate is produced and held by the process
//! that will serve it, and never has to be handed back across a library
//! boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use certon::acme_issuer::{AcmeIssuer, CertIssuer};
use certon::storage::Storage;
use gatel_plan::Acme as AcmeConfig;

use crate::acme::challenge::{Challenges, RouteSolver};
use crate::certificate::Certificate;

/// What an order produced: the material, and when it stops being usable.
#[derive(Debug, Clone)]
pub struct Issued {
    pub certificate: Certificate,
    pub chain_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// Unix seconds. Compared against the clock rather than carried as a
    /// duration, because a process can outlive the certificate it started with.
    pub not_after: i64,
}

/// The account with a certificate authority, and the way to ask it for a
/// certificate.
pub struct Issuer {
    inner: Arc<AcmeIssuer>,
    /// Where the account key and the issued material are kept.
    directory: PathBuf,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// Open the account. Nothing reaches the network here — the first order
    /// does that — so a configuration naming an unreachable CA still starts.
    pub fn new(config: &AcmeConfig, root: &Path, challenges: Arc<Challenges>) -> Result<Self, String> {
        let directory = match &config.directory_path {
            Some(path) => root.join(path),
            None => root.join("acme"),
        };
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create `{}`: {error}", directory.display()))?;

        let storage: Arc<dyn Storage> = Arc::new(certon::FileStorage::new(&directory));
        let inner = AcmeIssuer::builder()
            .ca(config.directory.clone())
            .email(config.email.clone())
            .agreed(config.agreed)
            .storage(storage)
            // Ours, so the challenge is answered by the listener the
            // configuration declares rather than by a second server bound
            // where nobody asked.
            .http01_solver(Arc::new(RouteSolver::new(challenges)))
            // Nothing else is wired up, and a CA that picked one of them would
            // wait for an answer that never comes.
            .disable_tlsalpn_challenge(true)
            .build();

        Ok(Self {
            inner: Arc::new(inner),
            directory,
        })
    }

    /// Ask for a certificate covering one name.
    pub async fn issue(&self, name: &str) -> Result<Issued, String> {
        let (csr, key_pem) = request_for(name)?;
        let issued = self
            .inner
            .issue(csr.as_ref(), std::slice::from_ref(&name.to_string()))
            .await
            .map_err(|error| format!("the certificate authority refused: {error}"))?;

        // The key is ours unless the issuer generated one of its own, which it
        // does not when handed a request.
        let key_pem = if issued.private_key_pem.is_empty() {
            key_pem.into_bytes()
        } else {
            issued.private_key_pem
        };
        let certificate = Certificate::from_pem(&issued.certificate_pem, &key_pem, "acme")?;
        let not_after = expiry(&issued.certificate_pem, &key_pem)?;

        Ok(Issued {
            certificate,
            chain_pem: issued.certificate_pem,
            key_pem,
            not_after,
        })
    }

    /// Where a name's material is kept between runs.
    pub fn paths(&self, name: &str) -> (PathBuf, PathBuf) {
        let safe = safe_name(name);
        (
            self.directory.join(format!("{safe}.crt")),
            self.directory.join(format!("{safe}.key")),
        )
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// A key and a request for it, both fresh.
///
/// A new key every time: reusing one across renewals means a key that has been
/// on disk for years protecting a certificate that is replaced every two
/// months.
fn request_for(
    name: &str,
) -> Result<(rustls::pki_types::CertificateSigningRequestDer<'static>, String), String> {
    let key = rcgen::KeyPair::generate().map_err(|error| format!("cannot generate a key: {error}"))?;
    let mut params = rcgen::CertificateParams::new(vec![name.to_string()])
        .map_err(|error| format!("`{name}` cannot be certified: {error}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let request = params
        .serialize_request(&key)
        .map_err(|error| format!("cannot build a certificate request: {error}"))?;
    Ok((request.der().clone(), key.serialize_pem()))
}

/// When the leaf stops being usable, in unix seconds.
fn expiry(chain_pem: &[u8], key_pem: &[u8]) -> Result<i64, String> {
    certon::Certificate::from_pem(chain_pem, key_pem)
        .map(|parsed| parsed.not_after.timestamp())
        .map_err(|error| format!("the issued certificate cannot be read: {error}"))
}

/// A host name as a file name, with nothing in it that could name a directory.
fn safe_name(name: &str) -> String {
    name.chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '.' => character,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gatel-acme-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn config(root: &Path) -> AcmeConfig {
        AcmeConfig {
            directory: "https://127.0.0.1:1/directory".to_string(),
            email: "ops@example.com".to_string(),
            agreed: true,
            directory_path: Some(root.join("state").display().to_string()),
            renew_before: std::time::Duration::from_secs(86_400),
        }
    }

    #[test]
    fn opening_the_account_touches_no_network() {
        // The CA here does not exist. Starting must still work, because a
        // gateway that cannot boot when its CA is down is worse than one
        // serving the certificates it already has.
        let root = directory("open");
        let issuer = Issuer::new(&config(&root), &root, Challenges::new()).unwrap();
        assert!(issuer.directory().is_dir(), "the state directory is created");
    }

    #[test]
    fn a_request_carries_the_name_and_a_key_of_its_own() {
        let (first, first_key) = request_for("example.com").unwrap();
        let (second, second_key) = request_for("example.com").unwrap();
        assert!(!first.as_ref().is_empty());
        assert_ne!(
            first_key, second_key,
            "a renewal must not reuse a key that has been on disk for years"
        );
        assert_ne!(first.as_ref(), second.as_ref());
        assert!(first_key.contains("PRIVATE KEY"));
    }

    #[test]
    fn a_name_cannot_escape_the_state_directory() {
        let root = directory("names");
        let issuer = Issuer::new(&config(&root), &root, Challenges::new()).unwrap();
        let (chain, key) = issuer.paths("../../etc/passwd");
        assert!(chain.starts_with(issuer.directory()), "{}", chain.display());
        assert!(key.starts_with(issuer.directory()));
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
    }

    #[test]
    fn each_name_has_its_own_files() {
        let root = directory("paths");
        let issuer = Issuer::new(&config(&root), &root, Challenges::new()).unwrap();
        let (chain, key) = issuer.paths("example.com");
        assert!(chain.ends_with("example.com.crt"));
        assert!(key.ends_with("example.com.key"));
        assert_ne!(issuer.paths("other.com").0, chain);
    }
}
