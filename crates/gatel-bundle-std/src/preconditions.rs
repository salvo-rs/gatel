//! Selected-representation preconditions (RFC 9110 section 13.2.2).
use std::time::SystemTime;

use http::{HeaderMap, HeaderName, Method, StatusCode, header};

pub(crate) fn single_date(headers: &HeaderMap, name: HeaderName) -> Option<SystemTime> {
    let mut values = headers.get_all(name).iter();
    let date = httpdate::parse_http_date(values.next()?.to_str().ok()?).ok()?;
    values.next().is_none().then_some(date)
}

fn tag(bytes: &[u8]) -> Option<(bool, &[u8])> {
    let weak = bytes.starts_with(b"W/");
    let bytes = bytes.strip_prefix(b"W/").unwrap_or(bytes);
    let opaque = bytes.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    opaque
        .iter()
        .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b) || *b >= 0x80)
        .then_some((weak, opaque))
}

pub(crate) fn strong_equal(left: &[u8], right: &[u8]) -> bool {
    matches!((tag(left), tag(right)), (Some((false, a)), Some((false, b))) if a == b)
}

pub(crate) fn valid_tag(bytes: &[u8]) -> bool {
    tag(bytes).is_some()
}

/// Match all field lines as one list. None means invalid syntax. Wildcards
/// cannot coexist with tags, even on different lines. A representation exists.
pub(crate) fn tags_match(
    headers: &HeaderMap,
    name: HeaderName,
    current: Option<&[u8]>,
    weak: bool,
) -> Option<bool> {
    let current = current.and_then(tag);
    let mut matched = false;
    let mut wildcards = 0;
    let mut tags = 0;
    for field in headers.get_all(name) {
        let mut input = field.as_bytes().trim_ascii();
        if input == b"*" {
            wildcards += 1;
            continue;
        }
        while !input.is_empty() {
            if input[0] == b',' {
                input = input[1..].trim_ascii_start();
                continue;
            }
            let prefix = usize::from(input.starts_with(b"W/")) * 2;
            if input.get(prefix) != Some(&b'"') {
                return None;
            }
            let end = prefix + 1 + input[prefix + 1..].iter().position(|b| *b == b'"')?;
            let (is_weak, opaque) = tag(&input[..=end])?;
            tags += 1;
            matched |= current.is_some_and(|(current_weak, value)| {
                value == opaque && (weak || (!is_weak && !current_weak))
            });
            input = input[end + 1..].trim_ascii_start();
            if !input.is_empty() && input[0] != b',' {
                return None;
            }
        }
    }
    if wildcards > 1 || (wildcards != 0 && tags != 0) {
        return None;
    }
    Some(wildcards == 1 || matched)
}

/// Origin evaluation after normal resource checks, before ranges. Invalid
/// entity tags are rejected; invalid or duplicated dates are ignored.
pub(crate) fn evaluate(method: &Method, request: &HeaderMap, selected: &HeaderMap) -> Option<StatusCode> {
    if matches!(*method, Method::CONNECT | Method::OPTIONS | Method::TRACE) {
        return None;
    }
    let current = selected.get(header::ETAG).map(|value| value.as_bytes());
    let modified = single_date(selected, header::LAST_MODIFIED);
    if request.contains_key(header::IF_MATCH) {
        match tags_match(request, header::IF_MATCH, current, false) {
            Some(true) => {}
            Some(false) => return Some(StatusCode::PRECONDITION_FAILED),
            None => return Some(StatusCode::BAD_REQUEST),
        }
    } else if let (Some(since), Some(modified)) =
        (single_date(request, header::IF_UNMODIFIED_SINCE), modified)
        && modified > since
    {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    let retrieval = matches!(*method, Method::GET | Method::HEAD);
    if request.contains_key(header::IF_NONE_MATCH) {
        match tags_match(request, header::IF_NONE_MATCH, current, true) {
            Some(true) => {
                return Some(if retrieval {
                    StatusCode::NOT_MODIFIED
                } else {
                    StatusCode::PRECONDITION_FAILED
                });
            }
            Some(false) => {}
            None => return Some(StatusCode::BAD_REQUEST),
        }
    } else if retrieval
        && let (Some(since), Some(modified)) = (single_date(request, header::IF_MODIFIED_SINCE), modified)
        && modified <= since
    {
        return Some(StatusCode::NOT_MODIFIED);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_preserve_commas_and_apply_strong_or_weak_comparison() {
        let mut headers = HeaderMap::new();
        headers.append(header::IF_MATCH, "W/\"a,b\", \"other\"".parse().unwrap());
        assert_eq!(
            tags_match(&headers, header::IF_MATCH, Some(b"\"a,b\""), true),
            Some(true)
        );
        assert_eq!(
            tags_match(&headers, header::IF_MATCH, Some(b"\"a,b\""), false),
            Some(false)
        );
        headers.append(header::IF_MATCH, "\"a,b\"".parse().unwrap());
        assert_eq!(
            tags_match(&headers, header::IF_MATCH, Some(b"\"a,b\""), false),
            Some(true)
        );
        assert_eq!(
            tags_match(&headers, header::IF_MATCH, Some(b"W/\"a,b\""), false),
            Some(false)
        );
        headers.append(header::IF_MATCH, "*".parse().unwrap());
        assert_eq!(
            tags_match(&headers, header::IF_MATCH, Some(b"\"a,b\""), true),
            None
        );
    }
    #[test]
    fn invalid_lists_never_short_circuit_after_a_match() {
        for input in [
            "\"a\",bad",
            "\"unterminated",
            "\"bad space\"",
            "*, \"a\"",
            "\"a\" \"b\"",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_MATCH, input.parse().unwrap());
            assert_eq!(
                tags_match(&headers, header::IF_MATCH, Some(b"\"a\""), true),
                None,
                "{input}"
            );
        }
    }
}
