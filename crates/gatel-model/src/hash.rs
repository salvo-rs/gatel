//! Deterministic fingerprints.
//!
//! Two hashes drive the whole transaction: `spec_hash` says whether the
//! operator asked for anything different, `plan_hash` says whether the compiled
//! behaviour differs. Both must be stable across process restarts, which is why
//! they are built from explicit length-prefixed field writes rather than
//! `#[derive(Hash)]`.

use std::fmt;

/// A 256-bit content fingerprint.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const ZERO: Self = Self([0; 32]);

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first 16 hex characters, which is what logs and `explain` show.
    pub fn short(&self) -> String {
        self.to_string()[..16].to_string()
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

/// Builds a [`Fingerprint`] from a sequence of typed writes.
///
/// Every write is length-prefixed and type-tagged, so `("ab", "c")` and
/// `("a", "bc")` cannot collide, and neither can the integer `1` and the string
/// `"1"`.
#[derive(Clone)]
pub struct Hasher {
    inner: blake3::Hasher,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    fn tag(&mut self, tag: u8) {
        self.inner.update(&[tag]);
    }

    pub fn update_str(&mut self, value: &str) -> &mut Self {
        self.tag(1);
        self.inner.update(&(value.len() as u64).to_le_bytes());
        self.inner.update(value.as_bytes());
        self
    }

    pub fn update_u64(&mut self, value: u64) -> &mut Self {
        self.tag(2);
        self.inner.update(&value.to_le_bytes());
        self
    }

    pub fn update_i64(&mut self, value: i64) -> &mut Self {
        self.tag(3);
        self.inner.update(&value.to_le_bytes());
        self
    }

    pub fn update_bool(&mut self, value: bool) -> &mut Self {
        self.tag(4);
        self.inner.update(&[u8::from(value)]);
        self
    }

    pub fn update_bytes(&mut self, value: &[u8]) -> &mut Self {
        self.tag(5);
        self.inner.update(&(value.len() as u64).to_le_bytes());
        self.inner.update(value);
        self
    }

    pub fn update_duration(&mut self, value: std::time::Duration) -> &mut Self {
        self.tag(6);
        self.inner.update(&value.as_nanos().to_le_bytes());
        self
    }

    /// Mark an absent optional field so `None` and a missing write differ.
    pub fn update_none(&mut self) -> &mut Self {
        self.tag(7);
        self
    }

    pub fn update_opt_str(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(text) => self.update_str(text),
            None => self.update_none(),
        }
    }

    pub fn update_fingerprint(&mut self, value: &Fingerprint) -> &mut Self {
        self.tag(8);
        self.inner.update(value.as_bytes());
        self
    }

    /// Combine an unordered collection: each element is hashed independently
    /// and the digests are sorted, so declaration order does not matter.
    pub fn update_unordered<T>(
        &mut self,
        items: &[T],
        mut hash_item: impl FnMut(&T, &mut Hasher),
    ) -> &mut Self {
        let mut digests: Vec<[u8; 32]> = items
            .iter()
            .map(|item| {
                let mut hasher = Hasher::new();
                hash_item(item, &mut hasher);
                *hasher.finish().as_bytes()
            })
            .collect();
        digests.sort_unstable();
        self.tag(9);
        self.inner.update(&(digests.len() as u64).to_le_bytes());
        for digest in digests {
            self.inner.update(&digest);
        }
        self
    }

    /// Combine an ordered collection, where position is part of the meaning.
    pub fn update_ordered<T>(
        &mut self,
        items: &[T],
        mut hash_item: impl FnMut(&T, &mut Hasher),
    ) -> &mut Self {
        self.tag(10);
        self.inner.update(&(items.len() as u64).to_le_bytes());
        for item in items {
            let mut hasher = Hasher::new();
            hash_item(item, &mut hasher);
            self.inner.update(hasher.finish().as_bytes());
        }
        self
    }

    pub fn finish(&self) -> Fingerprint {
        Fingerprint(*self.inner.finalize().as_bytes())
    }
}

/// Something that contributes to a fingerprint.
pub trait Fingerprinted {
    fn hash_into(&self, hasher: &mut Hasher);

    fn fingerprint(&self) -> Fingerprint {
        let mut hasher = Hasher::new();
        self.hash_into(&mut hasher);
        hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixing_prevents_concatenation_collisions() {
        let mut a = Hasher::new();
        a.update_str("ab").update_str("c");
        let mut b = Hasher::new();
        b.update_str("a").update_str("bc");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn type_tags_separate_integers_from_strings() {
        let mut a = Hasher::new();
        a.update_str("1");
        let mut b = Hasher::new();
        b.update_u64(1);
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn absent_optionals_differ_from_present_ones() {
        let mut a = Hasher::new();
        a.update_opt_str(None);
        let mut b = Hasher::new();
        b.update_opt_str(Some(""));
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn unordered_collections_ignore_declaration_order() {
        let forward = ["a", "b", "c"];
        let shuffled = ["c", "a", "b"];
        let hash = |items: &[&str]| {
            let mut hasher = Hasher::new();
            hasher.update_unordered(items, |item, hasher| {
                hasher.update_str(item);
            });
            hasher.finish()
        };
        assert_eq!(hash(&forward), hash(&shuffled));
    }

    #[test]
    fn ordered_collections_respect_position() {
        let forward = ["a", "b"];
        let reversed = ["b", "a"];
        let hash = |items: &[&str]| {
            let mut hasher = Hasher::new();
            hasher.update_ordered(items, |item, hasher| {
                hasher.update_str(item);
            });
            hasher.finish()
        };
        assert_ne!(hash(&forward), hash(&reversed));
    }

    #[test]
    fn short_form_is_sixteen_hex_characters() {
        let mut hasher = Hasher::new();
        hasher.update_str("gatel");
        let short = hasher.finish().short();
        assert_eq!(short.len(), 16);
        assert!(short.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
