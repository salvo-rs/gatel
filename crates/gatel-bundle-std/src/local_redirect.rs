//! Request-derived redirects must stay path-relative to the current origin.
use http::HeaderValue;

pub(crate) fn location(path: &str, query: Option<&str>) -> Option<HeaderValue> {
    // A network-path reference (//host) or browser-normalized backslash must
    // never turn a local path canonicalization into a different authority.
    let path = path
        .trim_start_matches('/')
        .replace('\\', "%5C")
        .replace('?', "%3F")
        .replace('#', "%23");
    let mut target = format!("/{path}");
    if let Some(query) = query {
        target.push('?');
        target.push_str(&query.replace('#', "%23"));
    }
    HeaderValue::from_str(&target).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_locations_cannot_replace_the_origin() {
        let base = url::Url::parse("https://gateway.example/old").unwrap();
        for path in [
            "//attacker.example/",
            "///attacker.example/",
            "/\\attacker.example/",
            "\\\\attacker.example",
            "",
            "////",
            "/%2F%2Fattacker.example/",
        ] {
            let target = location(path, Some("next=https%3A%2F%2Fother.example")).unwrap();
            let parsed = base.join(target.to_str().unwrap()).unwrap();
            assert_eq!(parsed.origin(), base.origin(), "{path}: {target:?}");
        }
    }
    #[test]
    fn query_and_encoded_path_bytes_are_preserved() {
        assert_eq!(
            location("//a%20b/", Some("a=1&a=2&next=%2Fdocs%3Fx%3D1")).unwrap(),
            "/a%20b/?a=1&a=2&next=%2Fdocs%3Fx%3D1"
        );
        assert_eq!(location("///", Some("")).unwrap(), "/?");
        assert!(location("/a\r\nb", None).is_none());
    }
}
