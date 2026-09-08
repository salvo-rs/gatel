//! Certon storage scoped to an already-held account lock.
//!
//! Its lease-file locking is redundant here and can leave a renewal task alive
//! when issuance is cancelled. The outer OS lock instead covers the entire
//! order. Mutations retain that lock in an owned task until the storage call
//! completes, even if the caller stops awaiting it.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use certon::error::{Error, Result};
use certon::storage::{KeyInfo, Storage};

pub(super) struct AccountStorage {
    inner: Arc<certon::FileStorage>,
    lock: Arc<File>,
    tasks: Arc<StorageTasks>,
    accounts: Arc<super::account_pair::Accounts>,
}

/// Persistent issuer-owned census: cancelling a drain never loses its work.
pub(super) struct StorageTasks(tokio::sync::watch::Sender<usize>);

impl Default for StorageTasks {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(0).0)
    }
}

struct Running(Arc<StorageTasks>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.0.send_modify(|count| *count -= 1);
    }
}

impl StorageTasks {
    pub(super) async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        self.0.send_modify(|count| *count += 1);
        let running = Running(self.clone());
        // Register before spawning, without a cancellation point between them.
        tokio::spawn(async move {
            let _running = running;
            operation.await
        })
        .await
        .map_err(|error| Error::Other(format!("account storage task failed: {error}")))?
    }

    /// The caller must first stop/join producers. This is not admission closure.
    pub(super) async fn drain(&self) {
        let mut count = self.0.subscribe();
        loop {
            if *count.borrow_and_update() == 0 {
                return;
            }
            if count.changed().await.is_err() {
                return;
            }
        }
    }
}

impl AccountStorage {
    /// `lock` must be the acquired OS lock for this directory.
    pub(super) fn new(directory: &Path, lock: Arc<File>, tasks: Arc<StorageTasks>) -> Self {
        Self {
            inner: Arc::new(certon::FileStorage::new(directory)),
            lock,
            tasks,
            accounts: Arc::new(super::account_pair::Accounts::new(directory)),
        }
    }

    async fn mutate<T: Send + 'static>(
        &self,
        operation: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let lock = self.lock.clone();
        self.tasks
            .run(async move {
                let _lock = lock;
                operation.await
            })
            .await
    }
}

#[async_trait::async_trait]
impl Storage for AccountStorage {
    async fn store(&self, key: &str, value: &[u8]) -> Result<()> {
        let inner = self.inner.clone();
        let key = key.to_owned();
        let value = value.to_vec();
        if let Some(account) = super::account_pair::key(&key)? {
            let accounts = self.accounts.clone();
            return self
                .mutate(async move { accounts.store(inner.as_ref(), account, &value).await })
                .await;
        }
        self.mutate(async move { inner.store(&key, &value).await }).await
    }

    async fn load(&self, key: &str) -> Result<Vec<u8>> {
        if let Some(account) = super::account_pair::key(key)? {
            return self.accounts.load(account, key);
        }
        self.inner.load(key).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let inner = self.inner.clone();
        let key = key.to_owned();
        if let Some(account) = super::account_pair::key(&key)? {
            let accounts = self.accounts.clone();
            return self
                .mutate(async move { accounts.delete(inner.as_ref(), account).await })
                .await;
        }
        self.mutate(async move { inner.delete(&key).await }).await
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        if super::account_pair::key(key)?.is_some() {
            return match self.load(key).await {
                Ok(_) => Ok(true),
                Err(Error::Storage(certon::error::StorageError::NotFound(_))) => Ok(false),
                Err(error) => Err(error),
            };
        }
        self.inner.exists(key).await
    }

    async fn list(&self, _path: &str, _recursive: bool) -> Result<Vec<String>> {
        Err(Error::Other(
            "enumeration is not supported by scoped ACME storage".into(),
        ))
    }

    async fn stat(&self, key: &str) -> Result<KeyInfo> {
        if let Some(account) = super::account_pair::key(key)? {
            let value = self.load(key).await?;
            let mut info = self.inner.stat(&account.record).await?;
            info.key = key.into();
            info.size = value.len() as u64;
            return Ok(info);
        }
        self.inner.stat(key).await
    }

    async fn lock(&self, _name: &str) -> Result<()> {
        // Only one order uses this adapter, already exclusively locked.
        Ok(())
    }

    async fn unlock(&self, _name: &str) -> Result<()> {
        // Scope exit, not a nested library call, releases the account lock.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn account_protocol_publishes_pairs_and_preserves_last_good_on_interruption() {
        const CA: &str = "https://fixture.example/directory";
        const EMAIL: &str = "ops@example.com";
        let directory = std::env::temp_dir().join(format!(
            "gatel-account-pair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let lock = super::super::account_lock::acquire(&directory).await.unwrap();
        let tasks = Arc::new(StorageTasks::default());
        let open = || AccountStorage::new(&directory, lock.clone(), tasks.clone());
        let mut storage = open();
        for unsupported in [
            "acme/issuer/users/ops@example.com/extra.json",
            "acme/issuer/users/ops@example.com/ops.pem",
            "acme/v2/issuer/users/ops@example.com/ops.json",
            "ACME/issuer/users/ops@example.com/ops.key",
            "acme\\issuer\\users\\ops@example.com\\ops.key",
            "other/../acme/issuer/users/ops@example.com/ops.key",
            "account-future-format.key",
            "issuer/challenge_tokens/example.com.json",
        ] {
            assert!(storage.store(unsupported, b"must not reach disk").await.is_err());
            assert!(storage.load(unsupported).await.is_err());
            assert!(storage.delete(unsupported).await.is_err());
            assert!(storage.exists(unsupported).await.is_err());
            assert!(storage.stat(unsupported).await.is_err());
            assert!(storage.list(unsupported, true).await.is_err());
        }
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "only the account lock exists"
        );
        let mut first = certon::account::new_account(EMAIL, certon::crypto::KeyType::default()).unwrap();
        first.location = "https://fixture.example/accounts/first".into();
        certon::account::save_account(&storage, CA, &first).await.unwrap();
        let loaded = certon::account::get_account(&storage, CA, EMAIL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.location, first.location);
        assert_eq!(loaded.private_key_pem, first.private_key_pem);
        let mut second = certon::account::new_account(EMAIL, certon::crypto::KeyType::default()).unwrap();
        second.location = "https://fixture.example/accounts/second".into();
        let registration = format!(
            "{}/ops.json",
            certon::storage::account_key_prefix(&certon::storage::issuer_key(CA), EMAIL)
        );
        storage
            .store(&registration, &serde_json::to_vec(&second).unwrap())
            .await
            .unwrap();
        // Simulate loss between the two library writes: only staging is lost.
        drop(storage);
        storage = open();
        let loaded = certon::account::get_account(&storage, CA, EMAIL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.location, first.location);
        assert_eq!(loaded.private_key_pem, first.private_key_pem);
        let mut broken = second.clone();
        broken.private_key_pem = "not a private key".into();
        assert!(
            certon::account::save_account(&storage, CA, &broken)
                .await
                .is_err()
        );
        let loaded = certon::account::get_account(&storage, CA, EMAIL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.location, first.location,
            "rollback must preserve the previous pair"
        );
        certon::account::save_account(&storage, CA, &second)
            .await
            .unwrap();
        let loaded = certon::account::get_account(&open(), CA, EMAIL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.location, second.location);
        assert_eq!(loaded.private_key_pem, second.private_key_pem);
        certon::account::delete_account_locally(&storage, CA, &second)
            .await
            .unwrap();
        assert!(
            certon::account::get_account(&open(), CA, EMAIL)
                .await
                .unwrap()
                .is_none()
        );
        // Old separate records have no atomic-pair evidence. Retain, do not import.
        certon::account::save_account(storage.inner.as_ref(), CA, &first)
            .await
            .unwrap();
        assert!(
            certon::account::get_account(&open(), CA, EMAIL)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!storage.exists(&registration).await.unwrap());
        certon::account::save_account(&storage, CA, &second)
            .await
            .unwrap();
        assert!(storage.exists(&registration).await.unwrap());
        assert_eq!(
            storage.stat(&registration).await.unwrap().size,
            storage.load(&registration).await.unwrap().len() as u64
        );
        let record = super::super::account_pair::key(&registration)
            .unwrap()
            .unwrap()
            .record;
        let path = directory.join(record);
        let complete = std::fs::read(&path).unwrap();
        for invalid in [
            complete[..complete.len() - 1].to_vec(),
            [complete.as_slice(), b"trailing"].concat(),
            b"broken".to_vec(),
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(
                certon::account::get_account(&open(), CA, EMAIL).await.is_err(),
                "corruption must not fall back to legacy or register again"
            );
        }
        std::fs::write(&path, complete).unwrap();
        certon::account::delete_account_locally(&storage, CA, &second)
            .await
            .unwrap();
        certon::account::delete_account_locally(storage.inner.as_ref(), CA, &first)
            .await
            .unwrap();
        drop(storage);
        drop(lock);
        std::fs::remove_file(directory.join("account.lock")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn failed_and_panicking_operations_do_not_strand_drains() {
        let tasks = Arc::new(StorageTasks::default());
        assert!(
            tasks
                .run(async { Err::<(), _>(Error::Other("fixture".into())) })
                .await
                .is_err()
        );
        assert!(
            tasks
                .run::<()>(async { panic!("fixture storage panic") })
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(tasks.drain(), tasks.drain());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_mutation_retains_account_lock_until_storage_finishes() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-account-storage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let lock = super::super::account_lock::acquire(&directory).await.unwrap();
        let tasks = Arc::new(StorageTasks::default());
        let storage = AccountStorage::new(&directory, lock, tasks.clone());
        storage.lock("nested").await.unwrap();
        storage.store("registration-intent.key", b"value").await.unwrap();
        assert_eq!(storage.load("registration-intent.key").await.unwrap(), b"value");
        assert_eq!(storage.stat("registration-intent.key").await.unwrap().size, 5);
        storage.unlock("nested").await.unwrap();
        assert!(!directory.join("locks").exists(), "no lease keepalive is created");
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(async move {
            storage
                .mutate(async move {
                    let _ = started.send(());
                    let _ = released.await;
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), tasks.drain())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                super::super::account_lock::acquire(&directory)
            )
            .await
            .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), tasks.drain())
            .await
            .unwrap();
        let lock = tokio::time::timeout(
            Duration::from_secs(2),
            super::super::account_lock::acquire(&directory),
        )
        .await
        .unwrap()
        .unwrap();
        let storage = AccountStorage::new(&directory, lock, tasks);
        storage.delete("registration-intent.key").await.unwrap();
        assert!(!storage.exists("registration-intent.key").await.unwrap());
        drop(storage);
        std::fs::remove_file(directory.join("account.lock")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
