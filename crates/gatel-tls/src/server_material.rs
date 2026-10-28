//! The exact server-side files read by a successfully prepared generation.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gatel_model::{Fingerprint, Hasher};

use crate::certificate::{Secrecy, read_bounded, read_optional};

const MAX_FILES: usize = 4096;

/// Content observations for manual server identities and inbound client CAs.
///
/// Only private fingerprints are retained, never the material itself. Missing,
/// unreadable or permission-invalid files request a refresh too: preparation
/// then reports the error without replacing the serving generation.
#[derive(Default)]
pub struct ServerMaterial {
    files: Mutex<BTreeMap<PathBuf, (Secrecy, Fingerprint)>>,
}

impl std::fmt::Debug for ServerMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerMaterial")
            .field(
                "files",
                &self.files.lock().unwrap_or_else(|p| p.into_inner()).len(),
            )
            .finish()
    }
}

impl ServerMaterial {
    pub(crate) fn read(&self, path: &Path, secrecy: Secrecy) -> Result<Vec<u8>, String> {
        let bytes = read_bounded(path, secrecy)?;
        let identity = fingerprint(&bytes);
        let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((previous_secrecy, previous)) = files.get_mut(path) {
            if *previous != identity {
                return Err("server TLS material changed during preparation; retry the reload".into());
            }
            if secrecy == Secrecy::Private {
                *previous_secrecy = secrecy;
            }
        } else {
            if files.len() >= MAX_FILES {
                return Err("too many server TLS material files in one generation".into());
            }
            files.insert(path.to_path_buf(), (secrecy, identity));
        }
        Ok(bytes)
    }

    /// Compare against the bytes actually parsed while preparing this runtime.
    /// Reads are bounded, and observations are not a cross-file atomic snapshot.
    pub fn changed(&self) -> bool {
        let files = self.files.lock().unwrap_or_else(|p| p.into_inner()).clone();
        files.iter().any(
            |(path, (secrecy, expected))| match read_optional(path, *secrecy) {
                Ok(Some(bytes)) => fingerprint(&bytes) != *expected,
                Ok(None) | Err(_) => true,
            },
        )
    }
}

fn fingerprint(bytes: &[u8]) -> Fingerprint {
    let mut hash = Hasher::new();
    hash.update_str("prepared-server-tls-material-v1");
    hash.update_bytes(bytes);
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_use_loaded_bytes_and_reject_inconsistent_duplicate_reads() {
        let directory = std::env::temp_dir().join(format!("gatel-server-material-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("public.pem");
        std::fs::write(&path, "first").unwrap();
        let material = ServerMaterial::default();
        assert_eq!(material.read(&path, Secrecy::Public).unwrap(), b"first");
        assert!(!material.changed());
        std::fs::write(&path, "second").unwrap();
        assert!(material.changed());
        assert!(material.read(&path, Secrecy::Public).is_err());
        assert!(!format!("{material:?}").contains("first"));
        std::fs::remove_file(&path).unwrap();
        assert!(material.changed());
        let _ = std::fs::remove_dir(directory);
    }
}
