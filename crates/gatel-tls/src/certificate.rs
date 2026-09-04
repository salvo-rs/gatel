//! Loading certificate material.
//!
//! Configuration names a file; it never contains a key. Everything here reads
//! from a path that a `SecretRef` already checked stays inside the
//! configuration directory.

use std::path::Path;
use std::sync::Arc;

use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;

/// A certificate chain with its private key, ready for rustls.
#[derive(Debug, Clone)]
pub struct Certificate {
    pub certified: Arc<CertifiedKey>,
    /// Where it came from, for events and `explain`. Never the key itself.
    pub source: String,
}

impl Certificate {
    /// Read a PEM chain and key from disk.
    ///
    /// The chain order matters and is preserved: the leaf first, then any
    /// intermediates. rustls sends them in the order given.
    pub fn from_pem_files(chain: &Path, key: &Path) -> Result<Self, String> {
        let chain_bytes = read_bounded(chain)?;
        let key_bytes = read_bounded(key)?;
        Self::from_pem(&chain_bytes, &key_bytes, &describe(chain))
    }

    pub fn from_pem(chain: &[u8], key: &[u8], source: &str) -> Result<Self, String> {
        let certificates: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut std::io::Cursor::new(chain))
                .collect::<Result<_, _>>()
                .map_err(|error| format!("cannot read the certificate chain: {error}"))?;
        if certificates.is_empty() {
            return Err("the certificate file contains no certificate".to_string());
        }

        let private_key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut std::io::Cursor::new(key))
            .map_err(|error| format!("cannot read the private key: {error}"))?
            .ok_or_else(|| "the key file contains no private key".to_string())?;

        // Building the signing key is also what proves the key belongs to the
        // certificate, and it happens here — during prepare — rather than on
        // the first handshake.
        let signing_key = any_supported_type(&private_key)
            .map_err(|error| format!("the private key is not usable: {error}"))?;
        let certified = CertifiedKey::new(certificates, signing_key);
        certified
            .keys_match()
            .map_err(|_| "the private key does not match the certificate".to_string())?;

        Ok(Self {
            certified: Arc::new(certified),
            source: source.to_string(),
        })
    }
}

/// Certificates are small. A file far larger than one is a mistake, or an
/// attempt to make the gateway read something it should not.
const MAX_PEM_BYTES: u64 = 256 * 1024;

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("cannot read `{}`: {error}", describe(path)))?;
    if !metadata.is_file() {
        return Err(format!("`{}` is not a regular file", describe(path)));
    }
    if metadata.len() > MAX_PEM_BYTES {
        return Err(format!(
            "`{}` is larger than {MAX_PEM_BYTES} bytes",
            describe(path)
        ));
    }
    check_permissions(path, &metadata)?;
    std::fs::read(path).map_err(|error| format!("cannot read `{}`: {error}", describe(path)))
}

/// A private key readable by everyone on the machine is a finding, not a
/// preference, so it is refused rather than warned about.
#[cfg(unix)]
fn check_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "`{}` is readable by group or others (mode {mode:o}); use 0600",
            describe(path)
        ));
    }
    let _ = metadata.uid();
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), String> {
    // Windows ACLs do not map onto a mode, and guessing would either reject
    // reasonable setups or give false assurance.
    Ok(())
}

fn describe(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed pair, generated rather than checked in, so the test can
    /// never fail because a fixture expired.
    fn pair(name: &str) -> (String, String) {
        let certified = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        (certified.cert.pem(), certified.signing_key.serialize_pem())
    }

    #[test]
    fn a_matching_pair_loads() {
        let (chain, key) = pair("example.com");
        let certificate = Certificate::from_pem(chain.as_bytes(), key.as_bytes(), "test").unwrap();
        assert_eq!(certificate.certified.cert.len(), 1);
    }

    #[test]
    fn a_key_from_a_different_certificate_is_rejected() {
        let (chain, _) = pair("example.com");
        let (_, other_key) = pair("other.example.com");
        let error = Certificate::from_pem(chain.as_bytes(), other_key.as_bytes(), "test").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn an_empty_or_malformed_file_is_rejected_with_which_half_failed() {
        let (chain, key) = pair("example.com");
        let error = Certificate::from_pem(b"", key.as_bytes(), "test").unwrap_err();
        assert!(error.contains("no certificate"), "{error}");

        let error = Certificate::from_pem(chain.as_bytes(), b"not a key", "test").unwrap_err();
        assert!(error.contains("no private key"), "{error}");
    }

    #[test]
    fn the_chain_order_is_preserved() {
        let (leaf, key) = pair("example.com");
        let (intermediate, _) = pair("ca.example.com");
        let combined = format!("{leaf}{intermediate}");
        let certificate = Certificate::from_pem(combined.as_bytes(), key.as_bytes(), "test").unwrap();
        assert_eq!(
            certificate.certified.cert.len(),
            2,
            "both certificates must be kept"
        );
    }

    #[test]
    fn a_missing_file_names_the_file_not_the_full_path() {
        let error =
            Certificate::from_pem_files(Path::new("/nowhere/secret.pem"), Path::new("/nowhere/k.pem"))
                .unwrap_err();
        assert!(error.contains("secret.pem"), "{error}");
        assert!(
            !error.contains("/nowhere/"),
            "a diagnostic should not leak the layout: {error}"
        );
    }
}
