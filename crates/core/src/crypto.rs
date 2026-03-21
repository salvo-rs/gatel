//! Cryptographic utilities.

/// Byte-level constant-time comparison to avoid timing side-channels.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn different_content() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn different_length() {
        assert!(!constant_time_eq(b"hello", b"hello!"));
    }
}
