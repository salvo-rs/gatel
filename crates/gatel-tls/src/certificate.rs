//! Loading certificate material.
//!
//! Configuration names a file; it never contains a key. Everything here reads
//! from a path that a `SecretRef` already checked stays inside the
//! configuration directory.

use std::path::Path;
use std::sync::Arc;

use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;

/// Read a PEM chain, in the order it was written.
///
/// `what` names the thing in every message this can produce, because "cannot
/// read the certificate" is not a useful sentence when a configuration has a
/// server certificate, a client identity and two authorities in it.
///
/// PEM parsing comes from `rustls-pki-types`, which is where rustls itself
/// keeps it. The separate `rustls-pemfile` crate was a thin wrapper over the
/// same code and is now unmaintained.
pub(crate) fn read_chain(bytes: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(bytes)
        .collect::<Result<_, _>>()
        .map_err(|error| format!("cannot read the {what}: {error}"))?;
    if certificates.is_empty() {
        return Err(format!("the {what} file contains no certificate"));
    }
    Ok(certificates)
}

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
        // The chain is public — it is sent to every client that connects — so
        // it is read without a permission check. The key is not, and is.
        let chain_bytes = read_bounded(chain, Secrecy::Public)?;
        let key_bytes = read_bounded(key, Secrecy::Private)?;
        Self::from_pem(&chain_bytes, &key_bytes, &describe(chain))
    }

    pub fn from_pem(chain: &[u8], key: &[u8], source: &str) -> Result<Self, String> {
        let certificates = read_chain(chain, "certificate chain")?;

        let private_key = PrivateKeyDer::from_pem_slice(key).map_err(|error| match error {
            pem::Error::NoItemsFound => "the key file contains no private key".to_string(),
            error => format!("cannot read the private key: {error}"),
        })?;

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

#[cfg(test)]
/// Write a private key the way anything that writes one should: readable by
/// nobody else.
///
/// Not a test convenience. Loading refuses a key that is readable by group or
/// others, so a helper that wrote one at the default mode would only ever have
/// worked on a platform where that check does nothing — which is exactly how
/// this went unnoticed.
pub fn write_key(path: &std::path::Path, pem: &str) {
    std::fs::write(path, pem).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Certificates are small. A file far larger than one is a mistake, or an
/// attempt to make the gateway read something it should not.
pub(crate) const MAX_PEM_BYTES: u64 = 256 * 1024;

/// Whether a file is something only this process should be able to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Secrecy {
    /// A certificate chain. Sent to every client that connects, so who else
    /// can read it on this machine is not a finding.
    Public,
    /// A private key.
    Private,
}

fn read_bounded(path: &Path, secrecy: Secrecy) -> Result<Vec<u8>, String> {
    read_optional(path, secrecy)?.ok_or_else(|| format!("cannot read `{}`: file not found", describe(path)))
}

pub(crate) fn read_optional(path: &Path, secrecy: Secrecy) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let failure = |error| format!("cannot read `{}`: {error}", describe(path));
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failure(error)),
    };
    if !metadata.is_file() {
        return Err(format!("`{}` is not a regular file", describe(path)));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failure(error)),
    };
    let metadata = file.metadata().map_err(failure)?;
    if !metadata.is_file() {
        return Err(format!("`{}` is not a regular file", describe(path)));
    }
    if metadata.len() > MAX_PEM_BYTES {
        return Err(format!(
            "`{}` is larger than {MAX_PEM_BYTES} bytes",
            describe(path)
        ));
    }
    if secrecy == Secrecy::Private {
        check_permissions(path, &metadata)?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_PEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() as u64 > MAX_PEM_BYTES {
        return Err("certificate material exceeds the size limit".into());
    }
    Ok(Some(bytes))
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

#[cfg(all(test, unix))]
mod permission_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn write(directory: &Path, name: &str, contents: &str, mode: u32) -> std::path::PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn directory(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("gatel-cert-perm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_world_readable_certificate_is_fine_because_it_is_public() {
        // It is sent to every client that connects. Refusing it for being
        // readable made `tls cert=... key=...` — the most basic way to serve
        // HTTPS — fail on every Unix machine, because nobody chmods a
        // certificate to 0600 and there is no reason to.
        let directory = directory("public-chain");
        let certified = rcgen::generate_simple_self_signed(vec!["example.com".to_string()]).unwrap();
        let chain = write(&directory, "cert.pem", &certified.cert.pem(), 0o644);
        let key = write(
            &directory,
            "key.pem",
            &certified.signing_key.serialize_pem(),
            0o600,
        );

        Certificate::from_pem_files(&chain, &key).expect("a public certificate is not a finding");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_world_readable_key_is_still_refused() {
        // The half that is worth refusing, and the reason the check exists.
        let directory = directory("public-key");
        let certified = rcgen::generate_simple_self_signed(vec!["example.com".to_string()]).unwrap();
        let chain = write(&directory, "cert.pem", &certified.cert.pem(), 0o644);
        let key = write(
            &directory,
            "key.pem",
            &certified.signing_key.serialize_pem(),
            0o644,
        );

        let error = Certificate::from_pem_files(&chain, &key).unwrap_err();
        assert!(error.contains("readable by group or others"), "{error}");
        assert!(error.contains("key.pem"), "and it names the key: {error}");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
