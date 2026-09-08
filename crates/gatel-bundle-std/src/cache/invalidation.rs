//! Resolve response links without allowing cross-origin cache eviction.
use http::{HeaderMap, header};

use super::store::Key;

pub(super) fn associated(request: &Key, headers: &HeaderMap) -> Vec<Key> {
    let Ok(base) = url::Url::parse(&format!(
        "{}://{}{}",
        request.scheme, request.host, request.target
    )) else {
        return Vec::new();
    };
    if !base.username().is_empty() || base.password().is_some() {
        return Vec::new();
    }
    let mut keys = Vec::new();
    for name in [header::LOCATION, header::CONTENT_LOCATION] {
        let mut values = headers.get_all(name).iter();
        let Some(value) = values.next().and_then(|v| v.to_str().ok()) else {
            continue;
        };
        if values.next().is_some() || value.bytes().any(|b| b <= 0x20 || b == 0x7f || b == b'\\') {
            continue;
        }
        let Ok(target) = base.join(value) else { continue };
        if target.origin() != base.origin() || !target.username().is_empty() || target.password().is_some() {
            continue;
        }
        let mut path = target.path().to_owned();
        if let Some(query) = target.query() {
            path.push('?');
            path.push_str(query);
        }
        let key = Key::new(&http::Method::GET, &request.scheme, &request.host, &path);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_links_resolve_but_different_origins_and_credentials_do_not() {
        let base = Key::new(&http::Method::POST, "https", "example.com", "/dir/write?old");
        for (location, expected) in [
            ("../read?q=1#fragment", Some("/read?q=1")),
            ("https://EXAMPLE.com:443/read", Some("/read")),
            ("?q=2", Some("/dir/write?q=2")),
            ("//other.example/read", None),
            ("http://example.com/read", None),
            ("https://example.com:444/read", None),
            ("https://user@example.com/read", None),
            (" /read", None),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::LOCATION, http::HeaderValue::from_static(location));
            let actual = associated(&base, &headers);
            assert_eq!(
                actual.first().map(|key| key.target.as_str()),
                expected,
                "{location}"
            );
        }
    }
}
