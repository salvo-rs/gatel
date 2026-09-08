//! Immutable validated password policies. No HTTP, configuration, or async runtime.
#![forbid(unsafe_code)]
#[cfg(feature = "argon2")]
mod argon2_policy;
mod bcrypt_policy;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Bcrypt,
    #[cfg(feature = "argon2")]
    Argon2id,
}
impl Algorithm {
    pub fn generation_limit(self) -> usize {
        match self {
            Self::Bcrypt => 71,
            #[cfg(feature = "argon2")]
            Self::Argon2id => 256,
        }
    }
}

pub fn algorithms() -> &'static [&'static str] {
    #[cfg(feature = "argon2")]
    {
        &["bcrypt", "argon2id"]
    }
    #[cfg(not(feature = "argon2"))]
    {
        &["bcrypt"]
    }
}

/// Construction validates parameters before any expensive computation is allowed.
pub struct Credential {
    algorithm: Algorithm,
    memory_bytes: u64,
    encoded: Zeroizing<String>,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(<redacted>)")
    }
}
impl Credential {
    pub fn parse(encoded: &str) -> Result<Self, &'static str> {
        let (algorithm, memory_bytes) = if bcrypt_policy::valid(encoded) {
            (Algorithm::Bcrypt, 8192)
        } else {
            #[cfg(feature = "argon2")]
            {
                let memory_bytes = argon2_policy::memory_bytes(encoded)
                    .ok_or("unsupported, malformed or over-budget password hash")?;
                (Algorithm::Argon2id, memory_bytes)
            }
            #[cfg(not(feature = "argon2"))]
            {
                return Err("unsupported, malformed or over-budget password hash (only bcrypt enabled)");
            }
        };
        Ok(Self {
            algorithm,
            memory_bytes,
            encoded: Zeroizing::new(encoded.to_owned()),
        })
    }
    /// Declared algorithm working memory for host admission. Argon2 uses the
    /// validated m parameter; bcrypt reserves 8 KiB. Not a process RSS estimate:
    /// excludes thread stacks, encoded material, allocator and runtime overhead.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }
    /// CPU work: caller must provide bounded execution outside async workers.
    pub fn verify(&self, password: &[u8]) -> bool {
        match self.algorithm {
            Algorithm::Bcrypt => {
                password.len() <= 72 && bcrypt::verify(password, self.encoded.as_str()).unwrap_or(false)
            }
            #[cfg(feature = "argon2")]
            Algorithm::Argon2id => {
                password.len() <= 256 && argon2_policy::verify(self.encoded.as_str(), password)
            }
        }
    }
}

/// Explicit generation policy: random salts, bounded input, no silent truncation.
pub fn generate(
    algorithm: Algorithm,
    password: &[u8],
    cost: Option<u32>,
) -> Result<Zeroizing<String>, &'static str> {
    if password.is_empty() || password.len() > algorithm.generation_limit() {
        return Err("password length exceeds algorithm policy");
    }
    match algorithm {
        Algorithm::Bcrypt => {
            let cost = cost.unwrap_or(12);
            if !(4..=16).contains(&cost) {
                return Err("bcrypt cost must be 4..16");
            }
            bcrypt::non_truncating_hash(password, cost)
                .map(Zeroizing::new)
                .map_err(|_| "cannot generate bcrypt hash")
        }
        #[cfg(feature = "argon2")]
        Algorithm::Argon2id => {
            if cost.is_some() {
                return Err("cost applies only to bcrypt");
            }
            argon2_policy::generate().and_then(|hasher| {
                use argon2::password_hash::PasswordHasher;
                hasher
                    .hash_password(password)
                    .map(|hash| Zeroizing::new(hash.to_string()))
                    .map_err(|_| "cannot generate argon2id hash")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bcrypt_policy_is_shared_and_material_is_redacted() {
        let hash = generate(Algorithm::Bcrypt, b"fixture", Some(4)).unwrap();
        let credential = Credential::parse(&hash).unwrap();
        assert_eq!(credential.memory_bytes(), 8192);
        assert!(credential.verify(b"fixture"));
        assert!(!credential.verify(b"wrong"));
        assert_eq!(format!("{credential:?}"), "Credential(<redacted>)");
        assert!(generate(Algorithm::Bcrypt, &[b'x'; 72], Some(4)).is_err());
    }

    #[cfg(feature = "argon2")]
    #[test]
    fn argon2id_generation_verification_and_parameter_budgets() {
        let hash = generate(Algorithm::Argon2id, b" fixture ", None).unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        let credential = Credential::parse(&hash).unwrap();
        assert!(credential.verify(b" fixture "));
        assert!(!credential.verify(b"fixture"));
        assert!(!credential.verify(&[b'x'; 257]));
        assert!(generate(Algorithm::Argon2id, b"x", Some(4)).is_err());
        for (from, to) in [
            ("argon2id", "argon2i"),
            ("v=19", "v=16"),
            ("m=19456", "m=32769"),
            ("m=19456", "m=8191"),
            ("t=2", "t=5"),
            ("t=2", "t=0"),
            ("p=1", "p=5"),
            ("m=19456,t=2,p=1", "m=19456,t=2"),
            ("p=1", "p=1,data=YQ"),
        ] {
            assert!(
                Credential::parse(&hash.replace(from, to)).is_err(),
                "{from} -> {to}"
            );
        }
    }

    #[cfg(not(feature = "argon2"))]
    #[test]
    fn unsupported_algorithms_are_not_silently_interpreted_as_bcrypt() {
        assert_eq!(algorithms(), &["bcrypt"]);
        assert!(Credential::parse("$argon2id$v=19$m=19456,t=2,p=1$fixture$fixture").is_err());
    }
}
