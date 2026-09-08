//! Representation metadata without path resolution or response publication.
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata-derived validator, not a content digest. Preserve the existing
/// wire format; same-size edits preserving mtime can collide.
pub(crate) fn etag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", metadata.len(), modified)
}

pub(crate) fn http_date(time: SystemTime) -> Option<String> {
    // httpdate 1.0 rejects pre-epoch times and panics at year 10000. Omit
    // Last-Modified outside its representable range instead of emitting an
    // invalid year or letting filesystem metadata panic the request task.
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    (seconds < 253_402_300_800).then(|| httpdate::fmt_http_date(time))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn dates_round_trip_at_calendar_and_supported_range_boundaries() {
        for (seconds, expected) in [
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            (951_782_400, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (4_107_542_400, "Mon, 01 Mar 2100 00:00:00 GMT"),
            (253_402_300_799, "Fri, 31 Dec 9999 23:59:59 GMT"),
        ] {
            let time = UNIX_EPOCH + Duration::from_secs(seconds);
            let formatted = http_date(time).unwrap();
            assert_eq!(formatted, expected);
            assert_eq!(httpdate::parse_http_date(&formatted).unwrap(), time);
            assert_eq!(
                http_date(time + Duration::from_nanos(999_999_999)).unwrap(),
                expected
            );
        }
        // Windows SystemTime rounds sub-100ns durations; use a representable
        // pre-epoch instant rather than accidentally constructing the epoch.
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert!(before_epoch < UNIX_EPOCH);
        assert!(http_date(before_epoch).is_none());
        assert!(http_date(UNIX_EPOCH + Duration::from_secs(253_402_300_800)).is_none());
    }
}
