//! Validate shape and work factor without performing password work at prepare.
pub(crate) fn valid(hash: &str) -> bool {
    let bytes = hash.as_bytes();
    if bytes.len() != 60 || !matches!(&bytes[..4], b"$2a$" | b"$2b$" | b"$2y$") || bytes[6] != b'$' {
        return false;
    }
    let Ok(cost) = hash[4..6].parse::<u32>() else {
        return false;
    };
    if !(4..=16).contains(&cost) || !bytes[4..6].iter().all(u8::is_ascii_digit) {
        return false;
    }
    const ALPHABET: &[u8] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    if !bytes[7..].iter().all(|byte| ALPHABET.contains(byte)) {
        return false;
    }
    // Canonical unused bits: 16-byte salt and 23-byte digest, respectively.
    ALPHABET.iter().position(|byte| *byte == bytes[28]).unwrap() % 16 == 0
        && ALPHABET.iter().position(|byte| *byte == bytes[59]).unwrap() % 4 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_hash_validation_never_computes_an_untrusted_cost() {
        let hash = bcrypt::hash("fixture", 4).unwrap();
        assert!(valid(&hash));
        for prefix in ["$2a$", "$2b$", "$2y$"] {
            assert!(valid(&format!("{prefix}{}", &hash[4..])));
        }
        for cost in ["00", "03", "17", "31", "+4"] {
            assert!(!valid(&format!("$2b${cost}${}", &hash[7..])));
        }
        assert!(!valid(&format!(" {hash}")));
        assert!(!valid(&hash.replace('$', "!")));
        let mut invalid = hash.into_bytes();
        invalid[28] = b'/';
        assert!(!valid(std::str::from_utf8(&invalid).unwrap()));
        assert!(!valid(&format!("$2b$é{}", "x".repeat(54))));
    }
}
