//! Certon's semantic store contract, backed by Gatel's atomic private records.
use std::path::{Path, PathBuf};

use certon::storage::CertificateResource;
use certon::{CertStore, Error, Result};

const MAX_STORAGE_NAME_BYTES: usize = 1024;

/// One account namespace, one issuer. The host coordinates writers and ownership;
/// this backend does not grant authority to publish into active TLS handshakes.
#[derive(Debug)]
pub struct AtomicCertStore {
    directory: PathBuf,
    issuer: String,
}

impl AtomicCertStore {
    /// Does not create directories or contact the network.
    pub fn new(directory: PathBuf, issuer: String) -> Self {
        Self { directory, issuer }
    }

    pub fn issuer_key(&self) -> &str {
        &self.issuer
    }

    fn check(&self, issuer: &str) -> Result<()> {
        if issuer != self.issuer {
            return Err(Error::Other("certificate store issuer mismatch".into()));
        }
        Ok(())
    }

    fn path(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() || name.len() > MAX_STORAGE_NAME_BYTES {
            return Err(Error::Other("invalid certificate storage name".into()));
        }
        let mut hash = gatel_model::Hasher::new();
        hash.update_str("certificate-resource-v1");
        hash.update_str(name);
        Ok(self
            .directory
            .join(format!("certificate-{}.identity", hash.finish())))
    }

    /// Synchronous preparation bridge; the same schema backs the async trait.
    pub fn load_cached(&self, domain: &str) -> Result<Option<CertificateResource>> {
        let bytes =
            crate::certificate::read_optional(&self.path(domain)?, crate::certificate::Secrecy::Private)
                .map_err(Error::Other)?;
        let Some(bytes) = bytes else {
            return self.legacy(domain);
        };
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Other("invalid certificate resource record".into()))?;
        if value["version"] != 1 {
            return Err(Error::Other("unsupported certificate resource version".into()));
        }
        if value["removed"] == true {
            return Ok(None);
        }
        let mut resource: CertificateResource = serde_json::from_value(value["resource"].take())
            .map_err(|_| Error::Other("invalid certificate resource metadata".into()))?;
        resource.certificate_pem = pem(&value, "chain")?;
        resource.private_key_pem = pem(&value, "key")?;
        self.validate(&resource)?;
        if resource.names_key() != domain {
            return Err(Error::Other("certificate resource name mismatch".into()));
        }
        Ok(Some(resource))
    }

    fn validate(&self, resource: &CertificateResource) -> Result<()> {
        self.check(&resource.issuer_key)?;
        if resource.sans.is_empty() {
            return Err(Error::Other("certificate resource has no names".into()));
        }
        // certon truncates the sorted, comma-joined name set. Hashing that
        // truncated key cannot restore uniqueness, so reject lossy keys before
        // parsing or writing. The same check protects records read from disk.
        let name_bytes = resource
            .sans
            .iter()
            .try_fold(resource.sans.len() - 1, |bytes, name| {
                bytes.checked_add(name.len())
            });
        if name_bytes.is_none_or(|bytes| bytes > MAX_STORAGE_NAME_BYTES) {
            return Err(Error::Other(
                "certificate name set exceeds the lossless storage key limit".into(),
            ));
        }
        let parsed = certon::Certificate::from_pem(&resource.certificate_pem, &resource.private_key_pem)?;
        let leaf = parsed
            .cert_chain
            .first()
            .ok_or_else(|| Error::Other("empty certificate chain".into()))?;
        let certificate = rustls::server::ParsedCertificate::try_from(leaf)
            .map_err(|_| Error::Other("invalid certificate leaf".into()))?;
        // certon's `names` includes CN as well as SANs; comparing that list with
        // resource.sans would reject valid certificates with an unrelated CN.
        // Use the TLS name verifier instead. Wildcard management is not exposed
        // by this backend: its names must be concrete DNS names or IP addresses.
        for name in &resource.sans {
            let name = rustls::pki_types::ServerName::try_from(name.as_str())
                .map_err(|_| Error::Other("certificate store requires concrete server names".into()))?;
            rustls::client::verify_server_name(&certificate, &name)
                .map_err(|_| Error::Other("certificate resource name is not covered by its SANs".into()))?;
        }
        crate::certificate::Certificate::from_pem(
            &resource.certificate_pem,
            &resource.private_key_pem,
            "certificate resource",
        )
        .map_err(Error::Other)?;
        Ok(())
    }

    fn legacy(&self, domain: &str) -> Result<Option<CertificateResource>> {
        // Only old canonical single-host records have an unambiguous filename.
        if !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            return Ok(None);
        }
        let (chain, key) =
            match super::identity_file::read(&self.directory.join(format!("{domain}.identity")))
                .map_err(Error::Other)?
            {
                Some(material) => (material.chain().to_vec(), material.key().to_vec()),
                None => {
                    let chain = crate::certificate::read_optional(
                        &self.directory.join(format!("{domain}.crt")),
                        crate::certificate::Secrecy::Public,
                    )
                    .map_err(Error::Other)?;
                    let key = read(&self.directory.join(format!("{domain}.key")))?;
                    match (chain, key) {
                        (None, None) => return Ok(None),
                        (Some(chain), Some(key)) => (chain, key),
                        _ => return Err(Error::Other("incomplete legacy certificate pair".into())),
                    }
                }
            };
        let resource = CertificateResource {
            sans: vec![domain.into()],
            certificate_pem: chain,
            private_key_pem: key,
            issuer_data: None,
            issuer_key: self.issuer.clone(),
        };
        self.validate(&resource)?;
        Ok(Some(resource))
    }
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    crate::certificate::read_optional(path, crate::certificate::Secrecy::Private).map_err(Error::Other)
}

fn pem(value: &serde_json::Value, field: &str) -> Result<Vec<u8>> {
    value[field]
        .as_str()
        .map(|pem| pem.as_bytes().to_vec())
        .ok_or_else(|| Error::Other("missing certificate resource PEM".into()))
}

#[async_trait::async_trait]
impl CertStore for AtomicCertStore {
    async fn load(&self, issuer: &str, domain: &str) -> Result<Option<CertificateResource>> {
        self.check(issuer)?;
        self.load_cached(domain)
    }

    async fn save(&self, issuer: &str, resource: &CertificateResource) -> Result<()> {
        self.check(issuer)?;
        self.validate(resource)?;
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1, "resource": resource,
            "chain": std::str::from_utf8(&resource.certificate_pem).map_err(|_| Error::Other("invalid certificate PEM encoding".into()))?,
            "key": std::str::from_utf8(&resource.private_key_pem).map_err(|_| Error::Other("invalid key PEM encoding".into()))?,
        })).map_err(|_| Error::Other("cannot encode certificate resource".into()))?;
        super::identity_file::publish(&self.path(&resource.names_key())?, &bytes).map_err(Error::Other)
    }

    async fn has(&self, issuer: &str, domain: &str) -> Result<bool> {
        Ok(self.load(issuer, domain).await?.is_some())
    }

    async fn remove(&self, issuer: &str, domain: &str) -> Result<()> {
        self.check(issuer)?;
        let path = self.path(domain)?;
        match std::fs::symlink_metadata(&self.directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(Error::Other("cannot inspect certificate store directory".into())),
            Ok(_) => {}
        }
        // A durable tombstone prevents legacy fallback from resurrecting a removal.
        super::identity_file::publish(&path, br#"{"version":1,"removed":true}"#).map_err(Error::Other)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn fixture() -> (PathBuf, AtomicCertStore) {
        let directory = std::env::temp_dir().join(format!(
            "gatel-certon-boundary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let store = AtomicCertStore::new(directory.clone(), "fixture".into());
        (directory, store)
    }

    fn resource(names: Vec<String>) -> CertificateResource {
        let cert = rcgen::generate_simple_self_signed(names.clone()).unwrap();
        CertificateResource {
            sans: names,
            certificate_pem: cert.cert.pem().into_bytes(),
            private_key_pem: cert.signing_key.serialize_pem().into_bytes(),
            issuer_key: "fixture".into(),
            issuer_data: None,
        }
    }

    #[tokio::test]
    async fn truncated_name_sets_cannot_alias_a_stored_certificate() {
        let (directory, store) = fixture();
        let names = (0..40)
            .map(|index| format!("{index:03}.abcdefghijklmnopqrst.example.com"))
            .collect();
        let first = resource(names);
        let mut second_names = first.sans.clone();
        second_names[39] = "zzz.different.example.com".into();
        let second = resource(second_names);
        // certon bounds names_key by truncation, not a collision-resistant hash.
        assert_eq!(first.names_key(), second.names_key());
        assert!(store.save("fixture", &first).await.is_err());
        assert!(store.save("fixture", &second).await.is_err());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        // A record written by an earlier backend must not bypass the guard.
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1, "resource": first,
            "chain": std::str::from_utf8(&first.certificate_pem).unwrap(),
            "key": std::str::from_utf8(&first.private_key_pem).unwrap(),
        }))
        .unwrap();
        let path = store.path(&first.names_key()).unwrap();
        super::super::identity_file::publish(&path, &bytes).unwrap();
        assert!(store.load("fixture", &first.names_key()).await.is_err());
        assert!(store.has("fixture", &first.names_key()).await.is_err());
        store.remove("fixture", &first.names_key()).await.unwrap();
        assert!(!store.has("fixture", &first.names_key()).await.unwrap());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn full_lossless_name_key_limit_is_usable() {
        let (directory, store) = fixture();
        let mut names: Vec<_> = (0..32)
            .map(|index| format!("{index:02}.{}.example.com", "x".repeat(16)))
            .collect();
        names[0].insert(0, 'x');
        assert_eq!(names.join(",").len(), MAX_STORAGE_NAME_BYTES);
        let resource = resource(names);
        store.save("fixture", &resource).await.unwrap();
        let loaded = store
            .load("fixture", &resource.names_key())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.sans, resource.sans);
        std::fs::remove_file(store.path(&resource.names_key()).unwrap()).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn multi_san_keys_are_order_independent_and_oversize_updates_preserve_the_record() {
        let (directory, store) = fixture();
        let original = resource(vec![
            "b.example.com".into(),
            "127.0.0.1".into(),
            "a.example.com".into(),
        ]);
        store.save("fixture", &original).await.unwrap();
        let path = store.path(&original.names_key()).unwrap();
        let complete = std::fs::read(&path).unwrap();
        let mut update = original.clone();
        update.sans.reverse();
        assert_eq!(update.names_key(), original.names_key());
        assert!(
            store
                .load("fixture", &update.names_key())
                .await
                .unwrap()
                .is_some()
        );
        update.issuer_data = Some(serde_json::Value::String("x".repeat(256 * 1024)));
        assert!(store.save("fixture", &update).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), complete);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        store.remove("fixture", &update.names_key()).await.unwrap();
        assert!(!store.has("fixture", &original.names_key()).await.unwrap());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    struct Issuer;
    #[async_trait::async_trait]
    impl certon::CertIssuer for Issuer {
        async fn issue(&self, _: &[u8], _: &[String]) -> Result<certon::IssuedCertificate> {
            Err(Error::Other("no network issuance in this test".into()))
        }
        fn issuer_key(&self) -> String {
            "fixture".into()
        }
    }

    #[tokio::test]
    async fn certon_store_roundtrip_preserves_metadata_and_removal_suppresses_legacy() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-certon-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(AtomicCertStore::new(directory.clone(), "fixture".into()));
        store.remove("fixture", "example.com").await.unwrap();
        assert!(
            !directory.exists(),
            "removing a missing resource does not create its store"
        );
        std::fs::create_dir(&directory).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let resource = CertificateResource {
            sans: vec!["example.com".into()],
            certificate_pem: cert.cert.pem().into_bytes(),
            private_key_pem: cert.signing_key.serialize_pem().into_bytes(),
            issuer_key: "fixture".into(),
            issuer_data: Some(serde_json::json!({"order": "fixture-order", "nested": [1,2,3]})),
        };
        let contract: Arc<dyn CertStore> = store.clone();
        assert!(!contract.has("fixture", "example.com").await.unwrap());
        contract.save("fixture", &resource).await.unwrap();
        let loaded = contract.load("fixture", "example.com").await.unwrap().unwrap();
        assert_eq!(loaded.issuer_data, resource.issuer_data);
        assert_eq!(loaded.private_key_pem, resource.private_key_pem);
        let manager = certon::CertManager::builder()
            .storage(Arc::new(certon::FileStorage::new(&directory)))
            .certificates(contract.clone())
            .issuers(vec![Arc::new(Issuer)])
            .build();
        assert!(manager.load_certificate("example.com").await.unwrap().is_some());
        assert!(contract.load("other-issuer", "example.com").await.is_err());
        let mut invalid = resource.clone();
        invalid.sans = vec!["wrong.example.com".into()];
        assert!(contract.save("fixture", &invalid).await.is_err());
        assert_eq!(
            contract
                .load("fixture", "example.com")
                .await
                .unwrap()
                .unwrap()
                .issuer_data,
            resource.issuer_data
        );
        // New corruption must not be disguised by a legacy identity.
        let legacy = directory.join("example.com.identity");
        super::super::identity_file::write(&legacy, &resource.certificate_pem, &resource.private_key_pem)
            .unwrap();
        let path = store.path("example.com").unwrap();
        let complete = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"broken").unwrap();
        assert!(contract.load("fixture", "example.com").await.is_err());
        std::fs::write(&path, complete).unwrap();
        contract.remove("fixture", "example.com").await.unwrap();
        assert!(contract.load("fixture", "example.com").await.unwrap().is_none());
        contract.remove("fixture", "example.com").await.unwrap();
        assert!(
            legacy.exists(),
            "removal does not destroy operator recovery material"
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(legacy).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
