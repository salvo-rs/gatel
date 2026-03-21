//! Pluggable storage abstraction for certificate and persistent data.
//!
//! The [`Storage`] trait defines a simple key-value interface that backends
//! must implement. The default [`FileStorage`] backend stores data on disk.

use std::io;
use std::path::{Path, PathBuf};

/// Trait for persistent key-value storage backends.
///
/// Keys are slash-separated paths (e.g. `"certs/example.com/cert.pem"`).
/// Values are raw bytes.
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// Store a value under the given key.
    async fn store(&self, key: &str, value: &[u8]) -> io::Result<()>;

    /// Load a value by key. Returns `None` if the key does not exist.
    async fn load(&self, key: &str) -> io::Result<Option<Vec<u8>>>;

    /// Delete a value by key.
    async fn delete(&self, key: &str) -> io::Result<()>;

    /// Check whether a key exists.
    async fn exists(&self, key: &str) -> io::Result<bool>;

    /// List all keys under a given prefix.
    async fn list(&self, prefix: &str) -> io::Result<Vec<String>>;
}

/// Filesystem-based storage backend.
///
/// Stores data in a directory tree under `root`. Keys are mapped to file
/// paths relative to the root.
pub struct FileStorage {
    root: PathBuf,
}

impl FileStorage {
    /// Create a new file storage rooted at the given directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn key_path(&self, key: &str) -> PathBuf {
        // Sanitize: prevent path traversal by resolving each component.
        let mut resolved = self.root.clone();
        for component in key.split(['/', '\\']) {
            match component {
                "" | "." | ".." => continue,
                c => resolved.push(c),
            }
        }
        resolved
    }
}

#[async_trait::async_trait]
impl Storage for FileStorage {
    async fn store(&self, key: &str, value: &[u8]) -> io::Result<()> {
        let path = self.key_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, value).await
    }

    async fn load(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        let path = self.key_path(key);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, key: &str) -> io::Result<()> {
        let path = self.key_path(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn exists(&self, key: &str) -> io::Result<bool> {
        let path = self.key_path(key);
        Ok(path.exists())
    }

    async fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let dir = self.key_path(prefix);
        let mut keys = Vec::new();
        if !dir.is_dir() {
            return Ok(keys);
        }
        collect_keys(&dir, &self.root, &mut keys).await?;
        Ok(keys)
    }
}

/// Recursively collect file paths relative to `root`.
async fn collect_keys(dir: &Path, root: &Path, keys: &mut Vec<String>) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            Box::pin(collect_keys(&path, root, keys)).await?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            keys.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path());

        storage.store("test/key.txt", b"hello").await.unwrap();
        assert!(storage.exists("test/key.txt").await.unwrap());

        let data = storage.load("test/key.txt").await.unwrap();
        assert_eq!(data.as_deref(), Some(b"hello".as_slice()));

        let keys = storage.list("test").await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].contains("key.txt"));

        storage.delete("test/key.txt").await.unwrap();
        assert!(!storage.exists("test/key.txt").await.unwrap());
    }

    #[tokio::test]
    async fn load_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path());
        assert_eq!(storage.load("nonexistent").await.unwrap(), None);
    }

    #[test]
    fn path_traversal_sanitized() {
        let root = std::env::temp_dir().join("gatel-test-root");
        let storage = FileStorage::new(&root);
        let path = storage.key_path("../../escape/test");
        assert!(
            path.starts_with(&root),
            "path traversal not prevented: {path:?}"
        );
        assert!(
            !path.to_string_lossy().contains(".."),
            "path contains '..': {path:?}"
        );
        // Should resolve to root/escape/test
        assert!(path.ends_with("escape/test") || path.ends_with("escape\\test"));
    }
}
