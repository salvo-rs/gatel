//! Install or remove a root CA certificate from the OS trust store.
//!
//! The goal is to mirror Caddy's `caddy trust` / `caddy untrust`: take the
//! root CA produced by [`crate::tls::local_ca::LocalCa`] and add it to the
//! current user's trust store so that browsers and HTTP clients accept locally
//! issued certificates without warnings.
//!
//! Platform notes:
//! - **Windows**: writes to the *current user* `Root` store via `schannel` — no UAC prompt.
//! - **macOS**: shells out to `security add-trusted-cert -d -r trustRoot -k
//!   ~/Library/Keychains/login.keychain-db`.
//! - **Linux**: copies the cert into `/usr/local/share/ca-certificates/` (Debian),
//!   `/etc/pki/ca-trust/source/anchors/` (RHEL), or `/etc/ca-certificates/trust-source/anchors/`
//!   (Arch) and runs the matching `update-ca-*` command. Requires root.

/// Result of a trust-store operation that the caller can present to the user.
#[derive(Debug)]
pub struct TrustOutcome {
    pub installed_path: Option<std::path::PathBuf>,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("trust-store operation failed: {0}")]
    Platform(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("this platform is not yet supported for automatic trust install")]
    Unsupported,
}

/// Install the given PEM-encoded root certificate into the OS trust store.
pub fn install(cert_pem: &str, cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
    platform::install(cert_pem, cert_der)
}

/// Remove a previously installed root certificate from the OS trust store.
///
/// Matching is done by SHA-256 fingerprint of `cert_der`.
pub fn uninstall(cert_pem: &str, cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
    platform::uninstall(cert_pem, cert_der)
}

#[cfg(windows)]
mod platform {
    use schannel::cert_context::{CertContext, HashAlgorithm};
    use schannel::cert_store::{CertAdd, CertStore};

    use super::*;

    pub fn install(_cert_pem: &str, cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let mut store = CertStore::open_current_user("Root")
            .map_err(|e| TrustError::Platform(format!("open current-user Root store: {e}")))?;
        let ctx = CertContext::new(cert_der)
            .map_err(|e| TrustError::Platform(format!("parse cert DER: {e}")))?;
        store
            .add_cert(&ctx, CertAdd::ReplaceExisting)
            .map_err(|e| TrustError::Platform(format!("add cert to store: {e}")))?;
        Ok(TrustOutcome {
            installed_path: None,
            message: "installed into Windows current-user Root store".into(),
        })
    }

    pub fn uninstall(_cert_pem: &str, cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let target = CertContext::new(cert_der)
            .map_err(|e| TrustError::Platform(format!("parse cert DER: {e}")))?;
        let target_fp = target
            .fingerprint(HashAlgorithm::sha256())
            .map_err(|e| TrustError::Platform(format!("fingerprint target cert: {e}")))?;

        let store = CertStore::open_current_user("Root")
            .map_err(|e| TrustError::Platform(format!("open current-user Root store: {e}")))?;

        let mut removed = 0usize;
        for cert in store.certs() {
            if let Ok(fp) = cert.fingerprint(HashAlgorithm::sha256()) {
                if fp == target_fp {
                    cert.delete()
                        .map_err(|e| TrustError::Platform(format!("delete cert: {e}")))?;
                    removed += 1;
                }
            }
        }
        Ok(TrustOutcome {
            installed_path: None,
            message: format!(
                "removed {removed} matching cert(s) from Windows current-user Root store"
            ),
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn write_temp_pem(cert_pem: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gatel-root-{}.pem", std::process::id()));
        std::fs::write(&path, cert_pem)?;
        Ok(path)
    }

    pub fn install(cert_pem: &str, _cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let pem_path = write_temp_pem(cert_pem)?;
        let home =
            std::env::var("HOME").map_err(|_| TrustError::Platform("$HOME not set".into()))?;
        let keychain = Path::new(&home).join("Library/Keychains/login.keychain-db");
        let status = Command::new("security")
            .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
            .arg(&keychain)
            .arg(&pem_path)
            .status()
            .map_err(|e| TrustError::Platform(format!("spawn `security`: {e}")))?;
        if !status.success() {
            return Err(TrustError::Platform(format!(
                "`security add-trusted-cert` exited with status {status}"
            )));
        }
        Ok(TrustOutcome {
            installed_path: Some(pem_path),
            message: format!(
                "installed into macOS keychain ({}); user may be prompted for password",
                keychain.display()
            ),
        })
    }

    pub fn uninstall(cert_pem: &str, _cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let pem_path = write_temp_pem(cert_pem)?;
        let status = Command::new("security")
            .args(["remove-trusted-cert"])
            .arg(&pem_path)
            .status()
            .map_err(|e| TrustError::Platform(format!("spawn `security`: {e}")))?;
        if !status.success() {
            return Err(TrustError::Platform(format!(
                "`security remove-trusted-cert` exited with status {status}"
            )));
        }
        Ok(TrustOutcome {
            installed_path: None,
            message: "removed from macOS keychain".into(),
        })
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    /// (anchor_dir, update_command) tuples in priority order.
    fn anchor_candidates() -> Vec<(PathBuf, &'static [&'static str])> {
        vec![
            (
                PathBuf::from("/usr/local/share/ca-certificates"),
                &["update-ca-certificates"],
            ),
            (
                PathBuf::from("/etc/pki/ca-trust/source/anchors"),
                &["update-ca-trust", "extract"],
            ),
            (
                PathBuf::from("/etc/ca-certificates/trust-source/anchors"),
                &["trust", "extract-compat"],
            ),
            (
                PathBuf::from("/usr/share/pki/trust/anchors"),
                &["update-ca-certificates"],
            ),
        ]
    }

    fn pick_anchor() -> Option<(PathBuf, &'static [&'static str])> {
        anchor_candidates().into_iter().find(|(p, _)| p.exists())
    }

    pub fn install(cert_pem: &str, _cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let (dir, update_cmd) = pick_anchor().ok_or_else(|| {
            TrustError::Platform(
                "no known CA anchor directory found; install ca-certificates package".into(),
            )
        })?;
        let dest = dir.join("gatel-local-ca.crt");
        std::fs::write(&dest, cert_pem).map_err(|e| {
            TrustError::Platform(format!(
                "write {}: {e} — re-run with sudo if this is a permissions error",
                dest.display()
            ))
        })?;
        let status = Command::new(update_cmd[0])
            .args(&update_cmd[1..])
            .status()
            .map_err(|e| TrustError::Platform(format!("spawn `{}`: {e}", update_cmd[0])))?;
        if !status.success() {
            return Err(TrustError::Platform(format!(
                "`{}` exited with status {status}",
                update_cmd[0]
            )));
        }
        Ok(TrustOutcome {
            installed_path: Some(dest.clone()),
            message: format!("installed at {}", dest.display()),
        })
    }

    pub fn uninstall(_cert_pem: &str, _cert_der: &[u8]) -> Result<TrustOutcome, TrustError> {
        let (dir, update_cmd) = pick_anchor()
            .ok_or_else(|| TrustError::Platform("no known CA anchor directory found".into()))?;
        let dest = dir.join("gatel-local-ca.crt");
        if dest.exists() {
            std::fs::remove_file(&dest).map_err(|e| {
                TrustError::Platform(format!(
                    "remove {}: {e} — re-run with sudo if this is a permissions error",
                    dest.display()
                ))
            })?;
        }
        let _ = Command::new(update_cmd[0]).args(&update_cmd[1..]).status();
        Ok(TrustOutcome {
            installed_path: None,
            message: format!("removed from {}", dir.display()),
        })
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;
    pub fn install(_pem: &str, _der: &[u8]) -> Result<TrustOutcome, TrustError> {
        Err(TrustError::Unsupported)
    }
    pub fn uninstall(_pem: &str, _der: &[u8]) -> Result<TrustOutcome, TrustError> {
        Err(TrustError::Unsupported)
    }
}
