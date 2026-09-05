//! `Range` and `If-Range`, straight off the wire.
//!
//! A range is arithmetic on a number a client chose, against a length it does
//! not know. Every branch either lands inside the file or refuses; nothing may
//! overflow, and nothing may produce a range that reads past the end.

#![no_main]

use libfuzzer_sys::fuzz_target;
use salvo_core::Request;

fuzz_target!(|data: &[u8]| {
    // The first byte picks a file size, so one input covers "empty file",
    // "small file" and "large file" without needing three targets.
    let (size, rest) = match data.split_first() {
        Some((first, rest)) => (u64::from(*first) * 4096, rest),
        None => return,
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let (range, if_range) = text.split_once('\n').unwrap_or((text, ""));

    let mut request = Request::default();
    if let Ok(value) = http::HeaderValue::from_str(range) {
        request.headers_mut().insert(http::header::RANGE, value);
    }
    if let Ok(value) = http::HeaderValue::from_str(if_range) {
        request.headers_mut().insert(http::header::IF_RANGE, value);
    }

    if let gatel_bundle_std::ranges::Wanted::Part { start, end } =
        gatel_bundle_std::ranges::wanted(&request, size, "\"etag\"")
    {
        assert!(start <= end, "an empty range was accepted: {start}-{end}");
        assert!(end < size, "a range past the end was accepted: {end} of {size}");
    }
});
