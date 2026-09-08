//! Representation metadata when dynamically changing content coding.
use http::{HeaderMap, HeaderValue, header};

pub(crate) fn no_transform(headers: &HeaderMap) -> bool {
    headers.get_all(header::CACHE_CONTROL).iter().any(|value| {
        value.to_str().map_or(true, |value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("no-transform"))
        })
    })
}

pub(crate) fn transformed(headers: &mut HeaderMap) {
    // These describe the original coded bytes; we do not recompute digests.
    for name in [
        "content-length",
        "accept-ranges",
        "content-md5",
        "digest",
        "content-digest",
        "repr-digest",
    ] {
        headers.remove(name);
    }
    if let Some(tag) = headers.remove(header::ETAG) {
        let bytes = tag.as_bytes();
        if bytes.starts_with(b"W/\"") && bytes.ends_with(b"\"") {
            headers.insert(header::ETAG, tag);
        } else if bytes.starts_with(b"\"") && bytes.ends_with(b"\"") {
            let mut weak = b"W/".to_vec();
            weak.extend_from_slice(bytes);
            if let Ok(tag) = HeaderValue::from_bytes(&weak) {
                headers.insert(header::ETAG, tag);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn representation_metadata_and_multiline_policy() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ETAG, HeaderValue::from_static("\"abc\""));
        headers.insert("content-digest", HeaderValue::from_static("old"));
        headers.insert("accept-ranges", HeaderValue::from_static("bytes"));
        transformed(&mut headers);
        assert_eq!(headers[header::ETAG], "W/\"abc\"");
        assert_eq!(headers.len(), 1);
        transformed(&mut headers);
        assert_eq!(headers[header::ETAG], "W/\"abc\"");
        headers.append(header::CACHE_CONTROL, HeaderValue::from_static("public"));
        assert!(!no_transform(&headers));
        headers.append(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=60, No-Transform"),
        );
        assert!(no_transform(&headers));
    }
}
