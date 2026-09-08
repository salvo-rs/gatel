//! Shared Vary parsing. Invalid dimensions fail closed as wildcard.
use http::header::VARY;
use http::{HeaderMap, HeaderName, HeaderValue};

pub(crate) fn names(headers: &HeaderMap) -> Vec<String> {
    let mut names = Vec::new();
    for value in headers.get_all(VARY) {
        let Ok(value) = value.to_str() else {
            return vec!["*".into()];
        };
        for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if name == "*" || HeaderName::from_bytes(name.as_bytes()).is_err() {
                return vec!["*".into()];
            }
            names.push(name.to_ascii_lowercase());
        }
    }
    names.sort();
    names.dedup();
    names
}

pub(crate) fn append(headers: &mut HeaderMap, name: &'static str) {
    let mut values = names(headers);
    if !values.iter().any(|v| v == "*" || v.eq_ignore_ascii_case(name)) {
        values.push(name.to_ascii_lowercase());
    }
    let value = HeaderValue::from_str(&values.join(", ")).unwrap_or(HeaderValue::from_static("*"));
    headers.insert(VARY, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_lines_duplicates_wildcard_and_invalid_values() {
        let mut headers = HeaderMap::new();
        headers.append(VARY, HeaderValue::from_static("Origin, Accept-Language"));
        headers.append(VARY, HeaderValue::from_static("ORIGIN"));
        append(&mut headers, "accept-encoding");
        assert_eq!(names(&headers), ["accept-encoding", "accept-language", "origin"]);
        headers.append(VARY, HeaderValue::from_static("*"));
        append(&mut headers, "origin");
        assert_eq!(names(&headers), ["*"]);
        for value in [
            HeaderValue::from_static("bad name"),
            HeaderValue::from_bytes(b"\xff").unwrap(),
        ] {
            headers.insert(VARY, value);
            assert_eq!(names(&headers), ["*"]);
        }
    }
}
