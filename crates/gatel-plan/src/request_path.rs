//! One decoded path identity for routing and authorization decisions.

use std::borrow::Cow;

/// Decode once and remove separator/dot aliases before choosing a route.
///
/// The wire URI is deliberately preserved: proxying, cache keys and redirects
/// still use its original escaping and query. File terminals decode that URI
/// once too, so its spelling cannot select a less restrictive route. Backslash
/// is treated as a separator on every platform, including when percent encoded.
pub fn routing_path(path: &str) -> Option<Cow<'_, str>> {
    if path == "*" {
        return Some(Cow::Borrowed(path));
    }
    if !path.starts_with('/') {
        return None;
    }
    let needs_normalizing = path.contains(['%', '\\'])
        || path.contains("//")
        || path.split('/').any(|part| matches!(part, "." | ".."));
    if !needs_normalizing {
        return (!path.chars().any(char::is_control)).then_some(Cow::Borrowed(path));
    }
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            index += 3;
            high * 16 + low
        } else {
            let byte = bytes[index];
            index += 1;
            byte
        };
        decoded.push(if byte == b'\\' { b'/' } else { byte });
    }
    let decoded = std::str::from_utf8(&decoded).ok()?;
    if decoded.chars().any(char::is_control) {
        return None;
    }
    let last = decoded.rsplit('/').next()?;
    let trailing_slash = decoded.ends_with('/') || matches!(last, "." | "..");
    let mut parts = Vec::new();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut canonical = format!("/{}", parts.join("/"));
    if trailing_slash && canonical.len() > 1 {
        canonical.push('/');
    }
    Some(Cow::Owned(canonical))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_have_one_identity_without_decoding_twice() {
        for path in [
            "/%70rivate/file",
            "//private/file",
            "/./private/file",
            "/public/../private/file",
            "/private%2Ffile",
            "/private%5cfile",
        ] {
            assert_eq!(routing_path(path).as_deref(), Some("/private/file"), "{path}");
        }
        assert_eq!(
            routing_path("/%2570rivate/file").as_deref(),
            Some("/%70rivate/file")
        );
        assert_eq!(routing_path("/a%20b/").as_deref(), Some("/a b/"));
        assert_eq!(routing_path("/private/.").as_deref(), Some("/private/"));
        assert_eq!(routing_path("/../").as_deref(), Some("/"));
        assert!(matches!(routing_path("/ordinary/file"), Some(Cow::Borrowed(_))));
    }

    #[test]
    fn malformed_escaping_and_controls_are_refused() {
        for path in ["/%", "/%0", "/%xy", "/%FF", "/%00", "/%0a", "/a\0b"] {
            assert!(routing_path(path).is_none(), "{path}");
        }
    }

    #[test]
    fn route_pattern_syntax_is_not_created_by_percent_decoding() {
        let literal = crate::PathMatch::parse("/literal%2A").unwrap();
        assert!(matches!(&literal, crate::PathMatch::Exact(path) if path == "/literal*"));
        assert!(!literal.matches("/literal-other"));
        let prefix = crate::PathMatch::parse("/literal*").unwrap();
        let mut a = gatel_model::Hasher::new();
        literal.hash_into(&mut a);
        let mut b = gatel_model::Hasher::new();
        prefix.hash_into(&mut b);
        assert_ne!(a.finish(), b.finish());
        assert_ne!(literal.describe(), prefix.describe());
        for pattern in ["/literal%2A", "/literal*", "/%25/%3F/%23", "/%25/%2A*"] {
            let parsed = crate::PathMatch::parse(pattern).unwrap();
            assert_eq!(crate::PathMatch::parse(&parsed.describe()).unwrap(), parsed);
        }
    }
}
