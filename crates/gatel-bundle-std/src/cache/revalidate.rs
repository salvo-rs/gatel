//! Single-owner stale GET revalidation. No detached tasks or stale-on-error.
use http::{HeaderMap, HeaderValue, header};
use salvo_core::http::{Request, ResBody, Response, StatusCode};

use super::store::Entry;

pub(super) fn eligible(request: &Request) -> bool {
    request.method() == http::Method::GET
        && ![
            header::IF_NONE_MATCH,
            header::IF_MODIFIED_SINCE,
            header::IF_MATCH,
            header::IF_UNMODIFIED_SINCE,
            header::RANGE,
            header::IF_RANGE,
        ]
        .iter()
        .any(|name| request.headers().contains_key(name))
}

pub(super) enum Validator {
    EntityTag(HeaderValue),
    ModifiedSince(HeaderValue),
}

impl Validator {
    pub(super) fn header(&self) -> http::HeaderName {
        match self {
            Self::EntityTag(_) => header::IF_NONE_MATCH,
            Self::ModifiedSince(_) => header::IF_MODIFIED_SINCE,
        }
    }

    pub(super) fn value(&self) -> HeaderValue {
        match self {
            Self::EntityTag(value) | Self::ModifiedSince(value) => value.clone(),
        }
    }
}

fn single(headers: &HeaderMap, name: http::HeaderName) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(value)
}

fn date(value: &HeaderValue) -> Result<std::time::SystemTime, ()> {
    httpdate::parse_http_date(value.to_str().map_err(|_| ())?).map_err(|_| ())
}

pub(super) fn validator(entry: &Entry) -> Option<Validator> {
    if entry.status != 200 || entry.headers.contains_key(header::CONTENT_ENCODING) {
        return None;
    }
    if let Some(tag) = single(&entry.headers, header::ETAG).ok()? {
        opaque(tag.as_bytes())?;
        return Some(Validator::EntityTag(tag.clone()));
    }
    let modified = single(&entry.headers, header::LAST_MODIFIED).ok()??;
    date(modified).ok()?;
    Some(Validator::ModifiedSince(modified.clone()))
}

fn opaque(bytes: &[u8]) -> Option<&[u8]> {
    let bytes = bytes.strip_prefix(b"W/").unwrap_or(bytes);
    let tag = bytes.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    tag.iter()
        .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b) || *b >= 0x80)
        .then_some(tag)
}

pub(super) fn merge(entry: &Entry, validator: &Validator, response: &mut Response) -> Result<(), ()> {
    let incoming = response.headers();
    if incoming.contains_key(header::CONTENT_RANGE) || incoming.contains_key(header::CONTENT_ENCODING) {
        return Err(());
    }
    let tag = single(incoming, header::ETAG)?;
    if let Some(tag) = tag {
        opaque(tag.as_bytes()).ok_or(())?;
    }
    let modified = single(incoming, header::LAST_MODIFIED)?;
    if let Some(modified) = modified {
        date(modified)?;
    }
    match validator {
        Validator::EntityTag(old) => {
            let new = tag.ok_or(())?;
            if opaque(new.as_bytes()) != opaque(old.as_bytes()) {
                return Err(());
            }
        }
        Validator::ModifiedSince(old) => {
            // A 304 need not repeat Last-Modified. If it does, conservatively
            // require the same instant, including equivalent obsolete formats.
            if let Some(new) = modified
                && date(new)? != date(old)?
            {
                return Err(());
            }
        }
    }
    if let Some(length) = single(incoming, header::CONTENT_LENGTH)? {
        let length: u64 = length.to_str().map_err(|_| ())?.parse().map_err(|_| ())?;
        if length != entry.body.len() as u64 {
            return Err(());
        }
    }
    let mut headers = entry.headers.clone();
    // Old freshness evidence must not survive a successful new validation.
    for name in [header::AGE, header::DATE] {
        headers.remove(name);
    }
    for name in incoming.keys() {
        headers.remove(name);
        for value in incoming.get_all(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    strip_transport(&mut headers);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&entry.body.len().to_string()).map_err(|_| ())?,
    );
    *response.headers_mut() = headers;
    response.status_code(StatusCode::OK);
    *response.body_mut() = ResBody::Once(entry.body.clone());
    Ok(())
}

fn strip_transport(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| http::HeaderName::from_bytes(v.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "cache-status",
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::LAST_MODIFIED,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        Entry {
            status: 200,
            headers,
            body: bytes::Bytes::from_static(b"seed"),
            stored: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(300),
            age_on_arrival: std::time::Duration::ZERO,
        }
    }

    #[test]
    fn valid_tag_takes_precedence_over_date() {
        let mut entry = entry();
        assert_eq!(validator(&entry).unwrap().header(), header::IF_MODIFIED_SINCE);
        entry
            .headers
            .insert(header::ETAG, HeaderValue::from_static("W/\"v1\""));
        let selected = validator(&entry).unwrap();
        assert_eq!(selected.header(), header::IF_NONE_MATCH);
        assert_eq!(selected.value(), "W/\"v1\"");
    }

    #[test]
    fn invalid_or_duplicate_tag_does_not_fall_back_to_date() {
        let mut entry = entry();
        entry
            .headers
            .insert(header::ETAG, HeaderValue::from_static("invalid"));
        assert!(validator(&entry).is_none());
        entry
            .headers
            .insert(header::ETAG, HeaderValue::from_static("\"v1\""));
        entry
            .headers
            .append(header::ETAG, HeaderValue::from_static("\"v1\""));
        assert!(validator(&entry).is_none());
    }

    #[test]
    fn invalid_or_duplicate_date_and_encoded_entries_are_not_candidates() {
        let mut candidate = entry();
        candidate.headers.append(
            header::LAST_MODIFIED,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert!(validator(&candidate).is_none());
        candidate
            .headers
            .insert(header::LAST_MODIFIED, HeaderValue::from_static("invalid"));
        assert!(validator(&candidate).is_none());
        let mut candidate = entry();
        candidate
            .headers
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert!(validator(&candidate).is_none());
    }

    #[test]
    fn duplicate_content_length_rejected_before_mutating_response() {
        let entry = entry();
        let selected = validator(&entry).unwrap();
        let mut response = Response::new();
        response.status_code(StatusCode::NOT_MODIFIED);
        response
            .headers_mut()
            .append(header::CONTENT_LENGTH, HeaderValue::from_static("4"));
        response
            .headers_mut()
            .append(header::CONTENT_LENGTH, HeaderValue::from_static("4"));
        assert!(merge(&entry, &selected, &mut response).is_err());
        assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
    }
}
