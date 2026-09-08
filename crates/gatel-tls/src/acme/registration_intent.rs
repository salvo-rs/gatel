//! Persist the account signing identity before any newAccount request.
use std::path::Path;

use certon::storage::Storage;

const FILE: &str = "registration-intent.key";

pub(super) async fn prepare(
    directory: &Path,
    storage: &dyn Storage,
    ca: &str,
    email: &str,
) -> Result<String, String> {
    // A corrupt committed account remains an error, not permission to create one.
    let account = certon::account::get_account(storage, ca, email)
        .await
        .map_err(|error| format!("cannot load account for registration recovery: {error}"))?;
    if let Some(bytes) =
        crate::certificate::read_optional(&directory.join(FILE), crate::certificate::Secrecy::Private)?
    {
        let key = String::from_utf8(bytes).map_err(|_| "invalid registration intent encoding")?;
        certon::crypto::decode_private_key_pem(&key)
            .map_err(|_| "invalid registration intent private key")?;
        if account.is_some_and(|account| account.private_key_pem != key) {
            return Err("registration intent conflicts with the committed account key".into());
        }
        return Ok(key);
    }
    let key = match account {
        Some(account) => account.private_key_pem,
        None => {
            certon::account::new_account(email, certon::crypto::KeyType::EcdsaP256)
                .map_err(|error| format!("cannot create registration identity: {error}"))?
                .private_key_pem
        }
    };
    storage
        .store(FILE, key.as_bytes())
        .await
        .map_err(|error| format!("cannot persist registration identity: {error}"))?;
    #[cfg(unix)]
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("cannot sync registration identity directory: {error}"))?;
    Ok(key)
}
