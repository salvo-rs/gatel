//! Corrected initial age. Monotonic delay plus wall-clock apparent age.
use std::time::{Duration, SystemTime};

use http::{HeaderMap, header};

pub(super) fn initial(headers: &HeaderMap, received: SystemTime, delay: Duration) -> Option<Duration> {
    let mut ages = headers.get_all(header::AGE).iter();
    let age = match ages.next() {
        None => Duration::ZERO,
        Some(value) => {
            let value = value.to_str().ok()?.trim();
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Duration::from_secs(value.parse().unwrap_or(u64::MAX))
        }
    };
    if ages.next().is_some() {
        return None;
    }
    let mut dates = headers.get_all(header::DATE).iter();
    let apparent = match dates.next() {
        None => Duration::ZERO,
        Some(value) => {
            let date = httpdate::parse_http_date(value.to_str().ok()?).ok()?;
            received.duration_since(date).unwrap_or_default()
        }
    };
    if dates.next().is_some() {
        return None;
    }
    Some(apparent.max(age.saturating_add(delay)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn apparent_age_delay_future_clock_and_invalid_headers() {
        let date = SystemTime::UNIX_EPOCH + Duration::from_secs(784111777);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::DATE,
            http::HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        headers.insert(header::AGE, http::HeaderValue::from_static("10"));
        assert_eq!(
            initial(&headers, date + Duration::from_secs(30), Duration::from_secs(5)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            initial(&headers, date, Duration::from_millis(500)),
            Some(Duration::from_millis(10500))
        );
        assert_eq!(
            initial(&headers, date - Duration::from_secs(30), Duration::ZERO),
            Some(Duration::from_secs(10))
        );
        headers.append(header::AGE, http::HeaderValue::from_static("20"));
        assert!(initial(&headers, date, Duration::ZERO).is_none());
        headers.insert(header::AGE, http::HeaderValue::from_static("-1"));
        assert!(initial(&headers, date, Duration::ZERO).is_none());
    }
}
