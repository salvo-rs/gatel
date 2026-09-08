//! Cooperative local-process exclusion for one account namespace.
//!
//! Never remove the lock file: unlinking a locked inode lets a second opener
//! lock a different file at the same path. Closing the handle releases the lock,
//! including when a waiting future is cancelled or its process exits.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn acquire(directory: &Path) -> Result<Arc<File>, String> {
    let path = directory.join("account.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("cannot open ACME account lock: {error}"))?;
    if !file.metadata().map_err(|error| error.to_string())?.is_file() {
        return Err("ACME account lock must be a regular file".into());
    }
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Arc::new(file)),
            Err(TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("cannot lock ACME account: {error}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_lock_holder() {
        let Some(directory) = std::env::var_os("GATEL_ACCOUNT_LOCK_TEST_DIRECTORY") else {
            return;
        };
        let directory = Path::new(&directory);
        let _lock = acquire(directory).await.unwrap();
        std::fs::write(directory.join("ready"), b"locked").unwrap();
        // The parent deliberately kills this process, exercising OS cleanup.
        std::future::pending::<()>().await;
    }

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn process_exclusion_cancellation_and_crash_release() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-account-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "acme::account_lock::tests::child_lock_holder",
                    "--nocapture",
                ])
                .env("GATEL_ACCOUNT_LOCK_TEST_DIRECTORY", &directory)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !directory.join("ready").exists() {
                assert!(child.0.try_wait().unwrap().is_none(), "lock holder exited early");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // Cancellation drops this waiter; it must not acquire a lock later.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), acquire(&directory))
                .await
                .is_err()
        );
        let other = directory.join("other-account");
        std::fs::create_dir(&other).unwrap();
        let independent = tokio::time::timeout(Duration::from_secs(1), acquire(&other))
            .await
            .unwrap()
            .unwrap();
        drop(independent);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let lock = tokio::time::timeout(Duration::from_secs(2), acquire(&directory))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), acquire(&directory))
                .await
                .is_err()
        );
        drop(lock);
        let reacquired = tokio::time::timeout(Duration::from_secs(1), acquire(&directory))
            .await
            .unwrap()
            .unwrap();
        drop(reacquired);
        // Remove only fixture-owned files after all processes/handles are done.
        std::fs::remove_file(directory.join("ready")).unwrap();
        std::fs::remove_file(directory.join("account.lock")).unwrap();
        std::fs::remove_file(other.join("account.lock")).unwrap();
        std::fs::remove_dir(other).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
