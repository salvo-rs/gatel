//! Generation-owned observations of secrets actually read during preparation.
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use gatel_model::{Fingerprint, Hasher, SecretRef, SecretString};

const LIMIT: usize = 4096;

#[derive(Default)]
pub struct SecretDependencies {
    entries: Mutex<Vec<(SecretRef, Fingerprint)>>,
    sealed: AtomicBool,
}

impl std::fmt::Debug for SecretDependencies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither secret values nor their digests are renderable metadata.
        f.debug_struct("SecretDependencies").finish_non_exhaustive()
    }
}

impl SecretDependencies {
    pub fn read(&self, reference: &SecretRef, root: &Path) -> Result<SecretString, String> {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if self.sealed.load(Ordering::Relaxed) {
            return Err("generation secret observations are sealed".into());
        }
        let existing = entries.iter().position(|(known, _)| known == reference);
        if existing.is_none() && entries.len() >= LIMIT {
            return Err("generation secret dependency limit exceeded".into());
        }
        let secret = gatel_model::resolve_secret(reference, root)?;
        let digest = digest(&secret);
        if let Some(index) = existing {
            if entries[index].1 != digest {
                return Err("secret changed during generation preparation".into());
            }
        } else {
            entries.push((reference.clone(), digest));
        }
        Ok(secret)
    }

    /// Missing, unreadable or changed values request a resource refresh. Only
    /// preparation validates the consuming plugin's format and publishes it.
    pub fn changed(&self, root: &Path) -> bool {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.iter().any(|(reference, expected)| {
            gatel_model::resolve_secret(reference, root)
                .map(|value| digest(&value) != *expected)
                .unwrap_or(true)
        })
    }

    /// Close collection once all plugins have prepared. Existing observers
    /// remain usable; any later read/registration is refused before I/O.
    pub fn seal(&self) {
        let _entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        self.sealed.store(true, Ordering::Relaxed);
    }
}

fn digest(value: &SecretString) -> Fingerprint {
    let mut hasher = Hasher::new();
    hasher.update_str("private-secret-observation-v1");
    hasher.update_str(value.expose());
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_exposes_neither_values_nor_comparison_digests() {
        let value = SecretString::new("SECRET-SENTINEL");
        let fingerprint = digest(&value);
        let dependencies = SecretDependencies::default();
        dependencies
            .entries
            .lock()
            .unwrap()
            .push((SecretRef::Env("PRIVATE_LOCATION".into()), fingerprint));
        let description = format!("{dependencies:?}");
        for hidden in ["SECRET-SENTINEL", "PRIVATE_LOCATION", &fingerprint.to_string()] {
            assert!(!description.contains(hidden));
        }
    }

    #[test]
    fn dependency_capacity_is_checked_before_reading_another_secret() {
        let dependencies = SecretDependencies::default();
        let mut entries = dependencies.entries.lock().unwrap();
        for index in 0..LIMIT {
            entries.push((SecretRef::Env(format!("CAPACITY_{index}")), Fingerprint::ZERO));
        }
        drop(entries);
        let error = dependencies
            .read(&SecretRef::Env("CAPACITY_NEW".into()), Path::new("."))
            .unwrap_err();
        assert_eq!(error, "generation secret dependency limit exceeded");
    }

    #[test]
    fn sealing_refuses_further_reads_without_disabling_change_observation() {
        let dependencies = SecretDependencies::default();
        let reference = SecretRef::Env("GATEL_DEFINITELY_UNSET_SEALED_TEST".into());
        dependencies
            .entries
            .lock()
            .unwrap()
            .push((reference.clone(), Fingerprint::ZERO));
        dependencies.seal();
        dependencies.seal();
        assert_eq!(
            dependencies.read(&reference, Path::new(".")).unwrap_err(),
            "generation secret observations are sealed"
        );
        assert!(dependencies.changed(Path::new(".")));
        assert_eq!(dependencies.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn repeated_reads_are_deduplicated_and_inconsistent_preparation_is_refused() {
        let path = std::env::temp_dir().join(format!("gatel-secret-observation-{}", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let root = path.parent().unwrap();
        let reference = SecretRef::File(path.file_name().unwrap().into());
        let dependencies = SecretDependencies::default();
        dependencies.read(&reference, root).unwrap();
        dependencies.read(&reference, root).unwrap();
        assert_eq!(dependencies.entries.lock().unwrap().len(), 1);
        assert!(!dependencies.changed(root));
        std::fs::write(&path, "new").unwrap();
        assert!(dependencies.changed(root));
        assert_eq!(
            dependencies.read(&reference, root).unwrap_err(),
            "secret changed during generation preparation"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(dependencies.changed(root));
    }
}
