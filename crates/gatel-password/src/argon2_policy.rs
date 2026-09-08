use argon2::password_hash::PasswordVerifier;
use argon2::password_hash::phc::PasswordHash;
use argon2::{Algorithm, Argon2, Params, Version};

fn parse(encoded: &str) -> Option<PasswordHash> {
    if encoded.len() > 256 {
        return None;
    }
    let hash = PasswordHash::new(encoded).ok()?;
    if hash.algorithm.as_str() != "argon2id" || hash.version != Some(19) {
        return None;
    }
    if hash.params.iter().count() != 3
        || hash
            .params
            .iter()
            .any(|(name, _)| !matches!(name.as_str(), "m" | "t" | "p"))
    {
        return None;
    }
    let params = Params::try_from(&hash).ok()?;
    if !(8192..=32768).contains(&params.m_cost())
        || !(1..=4).contains(&params.t_cost())
        || !(1..=4).contains(&params.p_cost())
    {
        return None;
    }
    if hash.salt.as_ref()?.len() != 16 || hash.hash.as_ref()?.len() != 32 {
        return None;
    }
    Some(hash)
}
pub(super) fn memory_bytes(encoded: &str) -> Option<u64> {
    let hash = parse(encoded)?;
    let params = Params::try_from(&hash).ok()?;
    Some(u64::from(params.m_cost()) * 1024)
}
pub(super) fn verify(encoded: &str, password: &[u8]) -> bool {
    parse(encoded).is_some_and(|hash| Argon2::default().verify_password(password, &hash).is_ok())
}
pub(super) fn generate() -> Result<Argon2<'static>, &'static str> {
    let params = Params::new(19456, 2, 1, Some(32)).map_err(|_| "invalid argon2id generation policy")?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}
