//! Explicit, bounded environment. Never inherit the host process environment.
use std::collections::BTreeMap;

use crate::Error;

pub struct Environment {
    values: BTreeMap<String, Vec<u8>>,
    bytes: usize,
    limit: usize,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("names", &self.values.keys())
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl Environment {
    pub const MAX_BYTES: usize = 1024 * 1024;
    pub fn new(limit: usize) -> Self {
        Self {
            values: BTreeMap::new(),
            bytes: 0,
            limit: limit.min(Self::MAX_BYTES),
        }
    }

    /// Duplicate names are rejected, not silently overwritten by request input.
    pub fn insert(&mut self, name: &str, value: &[u8]) -> Result<(), Error> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            || name.as_bytes()[0].is_ascii_digit()
            || value.contains(&0)
        {
            return Err(Error::InvalidEnvironment);
        }
        if self.values.contains_key(name) {
            return Err(Error::DuplicateEnvironment);
        }
        let added = name
            .len()
            .checked_add(value.len())
            .and_then(|n| n.checked_add(2))
            .ok_or(Error::Limit)?;
        let total = self
            .bytes
            .checked_add(added)
            .filter(|n| *n <= self.limit)
            .ok_or(Error::Limit)?;
        self.values.insert(name.to_owned(), value.to_vec());
        self.bytes = total;
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_slice()))
    }

    /// Explicitly replace an existing derived value. Failed validation or a
    /// budget overflow leaves the original value and accounting unchanged.
    /// Unlike insert, this cannot create a new name or hide duplicate input.
    pub fn replace_existing(&mut self, name: &str, value: &[u8]) -> Result<(), Error> {
        if value.contains(&0) {
            return Err(Error::InvalidEnvironment);
        }
        let previous = self.values.get_mut(name).ok_or(Error::InvalidEnvironment)?;
        let total = (self.bytes - previous.len())
            .checked_add(value.len())
            .filter(|total| *total <= self.limit)
            .ok_or(Error::Limit)?;
        *previous = value.to_vec();
        self.bytes = total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_is_atomic_and_accounts_for_growth_and_shrinkage() {
        let mut env = Environment::new(7);
        env.insert("A", b"1234").unwrap();
        assert!(env.replace_existing("A", b"12345").is_err());
        assert!(env.replace_existing("A", b"bad\0").is_err());
        assert!(env.replace_existing("MISSING", b"x").is_err());
        assert_eq!(env.iter().next().unwrap(), ("A", b"1234".as_slice()));
        assert_eq!(env.bytes, 7);
        env.replace_existing("A", b"").unwrap();
        assert_eq!(env.bytes, 3);
        env.insert("B", b"x").unwrap();
        assert_eq!(env.bytes, 7);
        assert!(env.insert("A", b"duplicate").is_err());
    }
}
