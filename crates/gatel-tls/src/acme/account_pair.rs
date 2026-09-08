//! Adapter for certon 0.3's registration-then-private-key save protocol.
//! The first write stages only memory; the second publishes one private record.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use certon::error::{Error, Result, StorageError};
use certon::storage::Storage;

const MAGIC: &[u8] = b"GATEL-ACCOUNT-1\n";
const LIMIT: usize = crate::certificate::MAX_PEM_BYTES as usize;

pub(super) struct Key {
    pub(super) record: String,
    registration: bool,
}

pub(super) fn key(key: &str) -> Result<Option<Key>> {
    if key.len() > 4096 || key.contains(['\\', ':']) {
        return Err(invalid("unsupported ACME storage path"));
    }
    let parts: Vec<_> = key.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(invalid("non-canonical ACME storage path"));
    }
    if !parts[0].eq_ignore_ascii_case("acme") {
        return if key == "registration-intent.key" {
            Ok(None)
        } else {
            Err(invalid("unsupported key in scoped ACME storage"))
        };
    }
    if parts.len() != 5 || parts[0] != "acme" || parts[2] != "users" {
        return Err(invalid("unsupported ACME account storage layout"));
    }
    let username = match parts[3].split_once('@') {
        Some(("", rest)) => rest,
        Some((name, _)) => name,
        None => parts[3],
    };
    let registration = if parts[4] == format!("{username}.json") {
        true
    } else if parts[4] == format!("{username}.key") {
        false
    } else {
        return Err(invalid("unsupported ACME account component"));
    };
    let mut hash = gatel_model::Hasher::new();
    hash.update_str("acme-account-record-v1");
    hash.update_str(key.rsplit_once('/').unwrap().0);
    Ok(Some(Key {
        record: format!("account-{}.key", hash.finish()),
        registration,
    }))
}

pub(super) struct Accounts {
    directory: PathBuf,
    pending: Mutex<BTreeMap<String, Vec<u8>>>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

impl Accounts {
    pub(super) fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
            pending: Mutex::default(),
        }
    }

    pub(super) async fn store(&self, inner: &dyn Storage, key: Key, value: &[u8]) -> Result<()> {
        if value.len() > LIMIT {
            return Err(invalid("ACME account component exceeds size limit"));
        }
        if key.registration {
            serde_json::from_slice::<certon::account::AcmeAccount>(value)
                .map_err(|_| invalid("invalid ACME account registration"))?;
            let mut pending = self.pending.lock().unwrap();
            if !pending.is_empty() {
                return Err(invalid("ACME registration save already pending"));
            }
            pending.insert(key.record, value.to_vec());
            return Ok(());
        }
        let registration = self
            .pending
            .lock()
            .unwrap()
            .get(&key.record)
            .cloned()
            .ok_or_else(|| invalid("ACME key write requires staged registration"))?;
        validate(&registration, value)?;
        let size = MAGIC.len() + 8 + registration.len() + value.len();
        if size > LIMIT {
            return Err(invalid("ACME account record exceeds size limit"));
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(registration.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&registration);
        bytes.extend_from_slice(value);
        // FileStorage syncs a private temporary file and atomically renames it.
        // Keep pending until every step succeeds: certon's rollback then removes
        // staging, never the previous (or just-published) complete record.
        inner.store(&key.record, &bytes).await?;
        #[cfg(unix)]
        std::fs::File::open(&self.directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| invalid(format!("ACME account directory sync failed: {error}")))?;
        self.pending.lock().unwrap().remove(&key.record);
        Ok(())
    }

    pub(super) fn load(&self, key: Key, original: &str) -> Result<Vec<u8>> {
        let bytes = crate::certificate::read_optional(
            &self.directory.join(&key.record),
            crate::certificate::Secrecy::Private,
        )
        .map_err(invalid)?
        .ok_or_else(|| Error::from(StorageError::NotFound(original.into())))?;
        let (registration, private) = decode(&bytes)?;
        Ok(if key.registration { registration } else { private }.to_vec())
    }

    pub(super) async fn delete(&self, inner: &dyn Storage, key: Key) -> Result<()> {
        if self.pending.lock().unwrap().remove(&key.record).is_some() {
            return Ok(()); // Roll back the staged save, not the last good account.
        }
        inner.delete(&key.record).await?;
        #[cfg(unix)]
        std::fs::File::open(&self.directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| invalid(format!("ACME account deletion sync failed: {error}")))?;
        Ok(())
    }
}

fn validate(registration: &[u8], private: &[u8]) -> Result<()> {
    serde_json::from_slice::<certon::account::AcmeAccount>(registration)
        .map_err(|_| invalid("invalid ACME account registration"))?;
    let pem = std::str::from_utf8(private).map_err(|_| invalid("invalid ACME account key encoding"))?;
    certon::crypto::decode_private_key_pem(pem).map_err(|_| invalid("invalid ACME account private key"))?;
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let header = MAGIC.len() + 8;
    if bytes.len() < header || bytes.len() > LIMIT || !bytes.starts_with(MAGIC) {
        return Err(invalid("invalid ACME account record header"));
    }
    let registration = u32::from_be_bytes(bytes[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap()) as usize;
    let private = u32::from_be_bytes(bytes[MAGIC.len() + 4..header].try_into().unwrap()) as usize;
    let end = header
        .checked_add(registration)
        .ok_or_else(|| invalid("invalid ACME account lengths"))?;
    if registration == 0 || private == 0 || end.checked_add(private) != Some(bytes.len()) {
        return Err(invalid("invalid ACME account lengths"));
    }
    let pair = (&bytes[header..end], &bytes[end..]);
    validate(pair.0, pair.1)?;
    Ok(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_canonical_account_components_share_a_record() {
        for email in [
            "ops@example.com",
            "ops_plus_tag@example.com",
            "default",
            "@example.com",
        ] {
            let username = match email.split_once('@') {
                Some(("", rest)) => rest,
                Some((name, _)) => name,
                None => email,
            };
            let prefix = format!("acme/issuer/users/{email}/{username}");
            assert_eq!(
                key(&format!("{prefix}.json")).unwrap().unwrap().record,
                key(&format!("{prefix}.key")).unwrap().unwrap().record
            );
        }
        assert!(key("registration-intent.key").unwrap().is_none());
        for invalid in [
            "",
            "/acme/a",
            "acme//users/u/u.key",
            "acme/a/users/../u.key",
            "acme/a/users/u/u.key:stream",
            "acme/a/users/u/u.key/",
        ] {
            assert!(key(invalid).is_err(), "{invalid}");
        }
        assert!(key(&"a".repeat(4097)).is_err());
    }
}
