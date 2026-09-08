//! Preconditions against a fresh selected representation, never stale metadata.
use http::{HeaderMap, header};

use super::store::Entry;
use crate::preconditions::{single_date, tags_match, valid_tag};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    Body,
    NotModified,
    Forward,
}

pub(super) fn evaluate(headers: &HeaderMap, entry: &Entry) -> Decision {
    if [
        header::IF_MATCH,
        header::IF_UNMODIFIED_SINCE,
        header::RANGE,
        header::IF_RANGE,
    ]
    .iter()
    .any(|name| headers.contains_key(name))
    {
        return Decision::Forward;
    }
    if headers.contains_key(header::IF_NONE_MATCH) {
        if entry.status != 200 {
            return Decision::Forward;
        }
        let mut values = entry.headers.get_all(header::ETAG).iter();
        let tag = values.next().map(|v| v.as_bytes());
        // A malformed or multi-valued origin validator is not evidence for
        // either a local 304 or an unconditional cached body. Ask the origin.
        if values.next().is_some() || tag.is_some_and(|value| !valid_tag(value)) {
            return Decision::Forward;
        }
        return match tags_match(headers, header::IF_NONE_MATCH, tag, true) {
            Some(true) => Decision::NotModified,
            Some(false) => Decision::Body,
            None => Decision::Forward,
        };
    }
    if headers.contains_key(header::IF_MODIFIED_SINCE) {
        if entry.status != 200 {
            return Decision::Forward;
        }
        if let (Some(since), Some(modified)) = (
            single_date(headers, header::IF_MODIFIED_SINCE),
            single_date(&entry.headers, header::LAST_MODIFIED),
        ) && modified <= since
        {
            return Decision::NotModified;
        }
    }
    Decision::Body
}
