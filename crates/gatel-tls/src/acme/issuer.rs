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
#[derive(Clone)]
pub struct Issued {
    pub certificate: Certificate,
    pub chain_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// Unix seconds. Compared against the clock rather than carried as a
    /// duration, because a process can outlive the certificate it started with.
    pub not_after: i64,
    pub issuer_data: Option<serde_json::Value>,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("chain_bytes", &self.chain_pem.len())
            .field("key", &"[REDACTED]")
            .field("not_after", &self.not_after)
            .field("has_issuer_data", &self.issuer_data.is_some())
            .finish()
    }
}

/// The account with a certificate authority, and the way to ask it for a
/// certificate.
pub struct Issuer {
    config: AcmeConfig,
    challenges: Arc<Challenges>,
    pub(super) storage_tasks: Arc<super::account_storage::StorageTasks>,
    /// Where the account key and the issued material are kept.
    directory: PathBuf,
    pub(super) certificates: super::AtomicCertStore,
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
        let directory = Self::storage_directory(config, root);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|error| format!("cannot create `{}`: {error}", directory.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&directory)
                .map_err(|error| format!("cannot inspect account directory: {error}"))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err("ACME account directory is accessible by group or others; use 0700".into());
            }
        }

        Ok(Self {
            config: config.clone(),
            challenges,
            storage_tasks: Arc::default(),
            certificates: super::AtomicCertStore::new(
                directory.clone(),
                certon::storage::issuer_key(&config.directory),
            ),
            directory,
        })
    }

    // Build after locking: certon's cached account must not outlive a turn
    // during which another process can replace account state on disk.
    async fn inner(&self, lock: Arc<std::fs::File>) -> Result<AcmeIssuer, String> {
        let storage: Arc<dyn Storage> = Arc::new(super::account_storage::AccountStorage::new(
            &self.directory,
            lock,
            self.storage_tasks.clone(),
        ));
        let registration_key = super::registration_intent::prepare(
            &self.directory,
            storage.as_ref(),
            &self.config.directory,
            &self.config.email,
        )
        .await?;
        Ok(AcmeIssuer::builder()
            .ca(self.config.directory.clone())
            .email(self.config.email.clone())
            .agreed(self.config.agreed)
            .storage(storage)
            // certon's account_key_pem builder field is not used by its account
            // creation path. The pre-registration callback is the actual hook.
            .new_account_func(Arc::new(move |account| {
                account.private_key_pem = registration_key.clone();
            }))
            // Ours, so the challenge is answered by the listener the
            // configuration declares rather than by a second server bound
            // where nobody asked.
            .http01_solver(Arc::new(RouteSolver::new(self.challenges.clone())))
            // HTTP-01 is owned by Challenges, not certon's disk-token handler.
            .disable_distributed_solvers(true)
            // Nothing else is wired up, and a CA that picked one of them would
            // wait for an answer that never comes.
            .disable_tlsalpn_challenge(true)
            .build())
    }

    /// Stable account namespace. Operational policy changes do not change keys.
    /// Unscoped legacy directories are deliberately not guessed or imported.
    pub fn storage_directory(config: &AcmeConfig, root: &Path) -> PathBuf {
        let base = match &config.directory_path {
            Some(path) => root.join(path),
            None => root.join("acme"),
        };
        let mut identity = gatel_model::Hasher::new();
        identity.update_str("acme-account-storage-v1");
        identity.update_str(&config.directory);
        identity.update_str(&config.email);
        base.join("accounts-v1").join(identity.finish().to_string())
    }

    /// Ask for a certificate covering one name.
    pub async fn issue(&self, name: &str) -> Result<Issued, String> {
        let account = super::account_lock::acquire(&self.directory).await?;
        self.issue_locked(name, account).await
    }

    /// Wait for already-started account mutations, including cancelled orders.
    /// Stop all callers of `issue` first; this does not close admission.
    pub async fn drain_storage(&self) {
        self.storage_tasks.drain().await;
    }

    /// Caller holds the account lock through its subsequent cache publication.
    pub(super) async fn issue_locked(
        &self,
        name: &str,
        account: Arc<std::fs::File>,
    ) -> Result<Issued, String> {
        let (csr, key_pem) = request_for(name)?;
        let issued = self
            .inner(account)
            .await?
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
            issuer_data: (!issued.metadata.is_null()).then_some(issued.metadata),
        })
    }

    /// Legacy two-file cache paths, read only when no identity record exists.
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

    /// Current atomic chain-and-key record.
    pub fn identity_path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{}.identity", safe_name(name)))
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
    fn account_namespaces_isolate_authorities_and_contacts_but_not_renewal_policy() {
        let root = directory("account-scopes");
        let original = config(&root);
        let path = Issuer::storage_directory(&original, &root);
        let mut changed = original.clone();
        changed.directory = "https://other.example/directory".into();
        assert_ne!(path, Issuer::storage_directory(&changed, &root));
        changed = original.clone();
        changed.email = "other@example.com".into();
        assert_ne!(path, Issuer::storage_directory(&changed, &root));
        changed = original;
        changed.renew_before *= 2;
        changed.agreed = false;
        assert_eq!(path, Issuer::storage_directory(&changed, &root));
        let issuer = Issuer::new(&changed, &root, Challenges::new()).unwrap();
        assert_eq!(issuer.directory(), path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[tokio::test]
    async fn registration_identity_survives_missing_response_and_rejects_conflicts() {
        let root = directory("registration-recovery");
        let config = config(&root);
        let issuer = Issuer::new(&config, &root, Challenges::new()).unwrap();
        let lock = super::super::account_lock::acquire(issuer.directory())
            .await
            .unwrap();
        let first = issuer.inner(lock.clone()).await.unwrap();
        let mut account =
            certon::account::new_account(&config.email, certon::crypto::KeyType::EcdsaP256).unwrap();
        first.new_account_func.as_ref().unwrap()(&mut account);
        let intended = account.private_key_pem.clone();
        assert_eq!(
            std::fs::read_to_string(issuer.directory().join("registration-intent.key")).unwrap(),
            intended
        );
        assert!(
            certon::account::get_account(first.storage.as_ref(), &config.directory, &config.email)
                .await
                .unwrap()
                .is_none()
        );
        // Registration may have reached the CA, but its response was lost.
        drop(first);
        let reopened = Issuer::new(&config, &root, Challenges::new()).unwrap();
        let second = reopened.inner(lock.clone()).await.unwrap();
        let mut recovered =
            certon::account::new_account(&config.email, certon::crypto::KeyType::EcdsaP256).unwrap();
        second.new_account_func.as_ref().unwrap()(&mut recovered);
        assert_eq!(recovered.private_key_pem, intended);
        recovered.location = "https://fixture.example/account/recovered".into();
        certon::account::save_account(second.storage.as_ref(), &config.directory, &recovered)
            .await
            .unwrap();
        drop(second);
        let third = reopened.inner(lock.clone()).await.unwrap();
        let loaded = certon::account::get_account(third.storage.as_ref(), &config.directory, &config.email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.location, recovered.location);
        assert_eq!(loaded.private_key_pem, intended);
        drop(third);
        let other = certon::account::new_account(&config.email, certon::crypto::KeyType::EcdsaP256).unwrap();
        crate::certificate::write_key(
            &issuer.directory().join("registration-intent.key"),
            &other.private_key_pem,
        );
        assert!(
            reopened
                .inner(lock.clone())
                .await
                .err()
                .unwrap()
                .contains("conflicts")
        );
        crate::certificate::write_key(&issuer.directory().join("registration-intent.key"), "broken");
        assert!(
            reopened
                .inner(lock)
                .await
                .err()
                .unwrap()
                .contains("invalid registration intent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_world_accessible_account_namespace_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = directory("account-permissions");
        let config = config(&root);
        let path = Issuer::storage_directory(&config, &root);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            Issuer::new(&config, &root, Challenges::new())
                .unwrap_err()
                .contains("0700")
        );
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
