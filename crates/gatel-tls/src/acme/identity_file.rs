//! A single bounded record, replaced atomically, never a separately updated pair.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::certificate::{Certificate, MAX_PEM_BYTES, Secrecy, read_optional};

const MAGIC: &[u8] = b"GATEL-IDENTITY-1\n";

pub(super) struct Material {
    bytes: Vec<u8>,
    chain_end: usize,
}
impl Material {
    pub fn chain(&self) -> &[u8] {
        &self.bytes[MAGIC.len() + 8..self.chain_end]
    }
    pub fn key(&self) -> &[u8] {
        &self.bytes[self.chain_end..]
    }
}

pub(super) fn read(path: &Path) -> Result<Option<Material>, String> {
    let Some(bytes) = read_optional(path, Secrecy::Private)? else {
        return Ok(None);
    };
    let header = MAGIC.len() + 8;
    if bytes.len() < header || !bytes.starts_with(MAGIC) {
        return Err("invalid identity file header".into());
    }
    let chain = u32::from_be_bytes(bytes[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap()) as usize;
    let key = u32::from_be_bytes(bytes[MAGIC.len() + 4..header].try_into().unwrap()) as usize;
    let end = header.checked_add(chain).ok_or("invalid identity lengths")?;
    if chain == 0 || key == 0 || end.checked_add(key) != Some(bytes.len()) {
        return Err("invalid identity lengths".into());
    }
    let material = Material {
        bytes,
        chain_end: end,
    };
    Certificate::from_pem(material.chain(), material.key(), "acme identity")?;
    Ok(Some(material))
}

struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
pub(super) fn write(path: &Path, chain: &[u8], key: &[u8]) -> Result<(), String> {
    write_with(path, chain, key, || Ok(()))
}

#[cfg(test)]
fn write_with(
    path: &Path,
    chain: &[u8],
    key: &[u8],
    before_commit: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), String> {
    let size = chain
        .len()
        .checked_add(key.len())
        .and_then(|size| size.checked_add(MAGIC.len() + 8));
    if size.is_none_or(|size| size as u64 > MAX_PEM_BYTES) {
        return Err("identity exceeds the size limit".into());
    }
    Certificate::from_pem(chain, key, "acme identity")?;
    let mut bytes = Vec::with_capacity(size.unwrap());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(chain.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&(key.len() as u32).to_be_bytes());
    bytes.extend_from_slice(chain);
    bytes.extend_from_slice(key);
    publish_with(path, &bytes, before_commit)
}

/// Shared private-file commit primitive; callers validate their record schema.
pub(super) fn publish(path: &Path, bytes: &[u8]) -> Result<(), String> {
    publish_with(path, bytes, || Ok(()))
}

fn publish_with(
    path: &Path,
    bytes: &[u8],
    before_commit: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), String> {
    if bytes.len() as u64 > MAX_PEM_BYTES {
        return Err("identity exceeds the size limit".into());
    }
    let parent = path.parent().ok_or("identity path has no parent")?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut staged = None;
    for _ in 0..32 {
        let temporary = parent.join(format!(
            ".gatel-identity-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => {
                staged = Some((Staged(temporary), file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot stage identity: {error}")),
        }
    }
    let (temporary, mut file) = staged.ok_or("cannot allocate a unique identity staging file")?;
    let save = || -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_commit()?;
        std::fs::rename(&temporary.0, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    save().map_err(|error| format!("cannot publish identity: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gatel-identity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("site.identity")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.path());
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    fn pair() -> (String, String) {
        let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        (generated.cert.pem(), generated.signing_key.serialize_pem())
    }

    #[test]
    fn failed_update_keeps_old_identity_and_cleans_temporary_private_material() {
        let root = Directory::new();
        let first = pair();
        let second = pair();
        write(&root.path(), first.0.as_bytes(), first.1.as_bytes()).unwrap();
        let result = write_with(&root.path(), second.0.as_bytes(), second.1.as_bytes(), || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for entry in std::fs::read_dir(&root.0)? {
                    assert_eq!(entry?.metadata()?.permissions().mode() & 0o777, 0o600);
                }
            }
            Err(std::io::Error::other("injected pre-publication failure"))
        });
        assert!(result.is_err());
        let loaded = read(&root.path()).unwrap().unwrap();
        assert_eq!(loaded.chain(), first.0.as_bytes());
        assert_eq!(loaded.key(), first.1.as_bytes());
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
        assert!(write(&root.path(), second.0.as_bytes(), first.1.as_bytes()).is_err());
        write(&root.path(), second.0.as_bytes(), second.1.as_bytes()).unwrap();
        let loaded = read(&root.path()).unwrap().unwrap();
        assert_eq!(loaded.chain(), second.0.as_bytes());
        assert!(certon::Certificate::from_pem(loaded.chain(), loaded.key()).is_ok());
    }

    #[test]
    fn concurrent_readers_never_observe_a_mixed_pair() {
        let root = Directory::new();
        let first = pair();
        let second = pair();
        write(&root.path(), first.0.as_bytes(), first.1.as_bytes()).unwrap();
        std::thread::scope(|scope| {
            for pair in [&first, &second] {
                let path = root.path();
                scope.spawn(move || {
                    for _ in 0..20 {
                        write(&path, pair.0.as_bytes(), pair.1.as_bytes()).unwrap();
                    }
                });
            }
            for _ in 0..100 {
                let loaded = read(&root.path()).unwrap().unwrap();
                assert!(loaded.chain() == first.0.as_bytes() || loaded.chain() == second.0.as_bytes());
                Certificate::from_pem(loaded.chain(), loaded.key(), "concurrent").unwrap();
            }
        });
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[test]
    fn malformed_lengths_truncation_and_trailing_data_are_refused() {
        let root = Directory::new();
        let pair = pair();
        write(&root.path(), pair.0.as_bytes(), pair.1.as_bytes()).unwrap();
        let original = std::fs::read(root.path()).unwrap();
        let mut large = original.clone();
        large[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut trailing = original.clone();
        trailing.push(0);
        for invalid in [
            original[..MAGIC.len()].to_vec(),
            original[..original.len() - 1].to_vec(),
            large,
            trailing,
        ] {
            std::fs::write(root.path(), invalid).unwrap();
            assert!(read(&root.path()).is_err());
        }
    }
}
