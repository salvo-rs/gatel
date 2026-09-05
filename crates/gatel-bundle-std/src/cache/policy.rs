//! What may be stored, what may be served, and for how long.
//!
//! Separated from the storage because this is the half that has to be right.
//! A cache that is merely slow is a performance problem; a cache that serves
//! one person's response to another is an incident, and every rule here exists
//! to keep the second from happening. It is therefore written to refuse when
//! it is unsure: not caching is always safe, and every branch that cannot
//! decide returns "no".

use std::time::Duration;

use salvo_core::http::{Request, Response, StatusCode};

/// Statuses a response may be stored under.
///
/// The heuristically cacheable set from RFC 9110, minus the ones that would be
/// wrong or useless here: 206 needs range bookkeeping this does not do, and
/// 304 is an answer about a cache entry rather than one to store.
const CACHEABLE: &[u16] = &[200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501];

/// Why a request or a response is not cached.
///
/// Carried into the `Cache-Status` header, because "why did this not cache"
/// is the question every cache leaves people asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a method whose answer can be reused.
    Method,
    /// The client asked for it not to be.
    ClientSaidNo,
    /// The response is not one of the statuses worth storing.
    Status,
    /// The response said `no-store`, `private` or `no-cache`.
    ResponseSaidNo,
    /// The response carries a `Set-Cookie`.
    ///
    /// A shared cache handing one person's cookie to the next is the worst
    /// thing a cache can do, and no header combination is worth the risk of
    /// getting the exception wrong.
    SetsACookie,
    /// The request was authorised, and the response did not say it may still
    /// be shared.
    Authorised,
    /// `Vary: *` means the response depends on something not in the request.
    VariesOnEverything,
    /// Nothing said how long it is good for.
    NoFreshness,
    /// Bigger than one entry is allowed to be.
    TooLarge,
}

impl Refusal {
    /// The `fwd-status`-style token for `Cache-Status`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Refusal::Method => "method",
            Refusal::ClientSaidNo => "request-no-store",
            Refusal::Status => "status",
            Refusal::ResponseSaidNo => "response-no-store",
            Refusal::SetsACookie => "set-cookie",
            Refusal::Authorised => "authorized",
            Refusal::VariesOnEverything => "vary-star",
            Refusal::NoFreshness => "no-freshness",
            Refusal::TooLarge => "too-large",
        }
    }
}

/// Whether this request may be answered from a cache at all.
pub fn request_is_cacheable(request: &Request) -> Result<(), Refusal> {
    if !matches!(*request.method(), http::Method::GET | http::Method::HEAD) {
        return Err(Refusal::Method);
    }
    if directive(header(request.headers(), http::header::CACHE_CONTROL), "no-store").is_some() {
        return Err(Refusal::ClientSaidNo);
    }
    Ok(())
}

/// Whether a stored entry may be handed to this request, as opposed to being
/// kept but not used.
pub fn may_serve_from_cache(request: &Request) -> bool {
    let control = header(request.headers(), http::header::CACHE_CONTROL);
    // `no-cache` means "check with the origin first". This cache does not
    // revalidate, so honouring it means going to the origin.
    directive(control, "no-cache").is_none() && directive(control, "max-age=0").is_none()
}

/// How long a response may be reused, or why it may not be stored.
///
/// `default_ttl` is what the configuration says to do with a response that
/// gives no freshness of its own. `None` means "then do not store it", which
/// is the default and is what nginx does without `proxy_cache_valid`.
pub fn freshness(
    request: &Request,
    response: &Response,
    default_ttl: Option<Duration>,
) -> Result<Duration, Refusal> {
    let status = response.status_code.unwrap_or(StatusCode::OK).as_u16();
    if !CACHEABLE.contains(&status) {
        return Err(Refusal::Status);
    }

    let headers = response.headers();
    let control = header(headers, http::header::CACHE_CONTROL);

    for refused in ["no-store", "private", "no-cache"] {
        if directive(control, refused).is_some() {
            return Err(Refusal::ResponseSaidNo);
        }
    }
    if headers.contains_key(http::header::SET_COOKIE) {
        return Err(Refusal::SetsACookie);
    }
    if header(headers, http::header::VARY).is_some_and(|vary| vary.split(',').any(|v| v.trim() == "*")) {
        return Err(Refusal::VariesOnEverything);
    }

    // A response to an authorised request is private unless it says otherwise.
    // RFC 9111 allows a few more directives here; `public` and `s-maxage` are
    // the two that unambiguously mean "a shared cache may keep this".
    if request.headers().contains_key(http::header::AUTHORIZATION)
        && directive(control, "public").is_none()
        && seconds(control, "s-maxage").is_none()
    {
        return Err(Refusal::Authorised);
    }

    // A shared cache prefers `s-maxage`, then `max-age`, then `Expires`.
    if let Some(seconds) = seconds(control, "s-maxage").or_else(|| seconds(control, "max-age")) {
        return if seconds == 0 {
            Err(Refusal::NoFreshness)
        } else {
            Ok(Duration::from_secs(seconds))
        };
    }
    if let Some(lifetime) = expires_in(response) {
        return if lifetime.is_zero() {
            Err(Refusal::NoFreshness)
        } else {
            Ok(lifetime)
        };
    }

    default_ttl.ok_or(Refusal::NoFreshness)
}

/// `Expires` against `Date`, which is how long the origin says it has left.
///
/// Both are read from the response rather than using the local clock, so a
/// gateway whose clock is off by a minute does not shorten or extend every
/// lifetime by a minute.
fn expires_in(response: &Response) -> Option<Duration> {
    let headers = response.headers();
    let expires = http_date(header(headers, http::header::EXPIRES)?)?;
    let date = header(headers, http::header::DATE)
        .and_then(http_date)
        .unwrap_or_else(now_seconds);
    Some(Duration::from_secs(expires.saturating_sub(date)))
}

/// The header names a stored entry has to match on, from `Vary`.
///
/// Lower-cased and sorted, so two responses that name the same headers in a
/// different order produce the same key.
pub fn vary_on(response: &Response) -> Vec<String> {
    let Some(vary) = header(response.headers(), http::header::VARY) else {
        return Vec::new();
    };
    let mut names: Vec<String> = vary
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn header(headers: &http::HeaderMap, name: http::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Whether a comma-separated directive list contains `wanted`.
fn directive(control: Option<&str>, wanted: &str) -> Option<()> {
    control?
        .split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case(wanted))
        .then_some(())
}

/// `name=seconds` out of a directive list.
fn seconds(control: Option<&str>, name: &str) -> Option<u64> {
    for entry in control?.split(',') {
        let entry = entry.trim();
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return value.trim().trim_matches('"').parse().ok();
        }
    }
    None
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Parse an HTTP date into seconds since the epoch.
///
/// Only the IMF-fixdate form, which is the one a server is required to send.
/// The two obsolete forms are not parsed: a response using one of them is
/// treated as having no `Expires`, which costs a cache entry rather than
/// risking a wrong lifetime.
fn http_date(text: &str) -> Option<u64> {
    // `Sun, 06 Nov 1994 08:49:37 GMT`
    let text = text.trim();
    let rest = text.get(5..)?;
    let mut parts = rest.split(' ');
    let day: u64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 0,
        "Feb" => 1,
        "Mar" => 2,
        "Apr" => 3,
        "May" => 4,
        "Jun" => 5,
        "Jul" => 6,
        "Aug" => 7,
        "Sep" => 8,
        "Oct" => 9,
        "Nov" => 10,
        "Dec" => 11,
        _ => return None,
    };
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;

    if year < 1970 {
        return None;
    }
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    const LENGTHS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for (index, length) in LENGTHS.iter().enumerate().take(month) {
        days += length + u64::from(index == 1 && is_leap(year));
    }
    days += day.checked_sub(1)?;
    Some(((days * 24 + hour) * 60 + minute) * 60 + second)
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: http::Method, headers: &[(&str, &str)]) -> Request {
        let mut request = Request::default();
        *request.method_mut() = method;
        for (name, value) in headers {
            request.headers_mut().insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        request
    }

    fn response(status: u16, headers: &[(&str, &str)]) -> Response {
        let mut response = Response::new();
        response.status_code(StatusCode::from_u16(status).unwrap());
        for (name, value) in headers {
            response.headers_mut().insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        response
    }

    fn plain() -> Request {
        request(http::Method::GET, &[])
    }

    #[test]
    fn only_methods_whose_answer_can_be_reused() {
        assert!(request_is_cacheable(&request(http::Method::GET, &[])).is_ok());
        assert!(request_is_cacheable(&request(http::Method::HEAD, &[])).is_ok());
        for method in [http::Method::POST, http::Method::PUT, http::Method::DELETE] {
            assert_eq!(request_is_cacheable(&request(method, &[])), Err(Refusal::Method));
        }
    }

    #[test]
    fn a_response_that_says_how_long_is_stored_for_that_long() {
        let fresh = freshness(
            &plain(),
            &response(200, &[("cache-control", "max-age=300")]),
            None,
        );
        assert_eq!(fresh, Ok(Duration::from_secs(300)));
    }

    #[test]
    fn a_shared_cache_prefers_s_maxage() {
        // It exists precisely to tell a shared cache something different from
        // what a browser is told.
        let fresh = freshness(
            &plain(),
            &response(200, &[("cache-control", "max-age=60, s-maxage=600")]),
            None,
        );
        assert_eq!(fresh, Ok(Duration::from_secs(600)));
    }

    #[test]
    fn a_response_that_says_nothing_is_not_stored_unless_told() {
        assert_eq!(
            freshness(&plain(), &response(200, &[]), None),
            Err(Refusal::NoFreshness)
        );
        assert_eq!(
            freshness(&plain(), &response(200, &[]), Some(Duration::from_secs(30))),
            Ok(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_response_that_says_no_is_never_stored_however_it_says_it() {
        for said in ["no-store", "private", "no-cache", "public, no-store"] {
            assert_eq!(
                freshness(
                    &plain(),
                    &response(200, &[("cache-control", said)]),
                    Some(Duration::from_secs(60)),
                ),
                Err(Refusal::ResponseSaidNo),
                "{said}"
            );
        }
    }

    #[test]
    fn a_response_that_sets_a_cookie_is_never_stored() {
        // The worst thing a shared cache can do is hand one person's session
        // to the next, and no header combination is worth the risk of getting
        // the exception wrong.
        assert_eq!(
            freshness(
                &plain(),
                &response(
                    200,
                    &[("cache-control", "public, max-age=600"), ("set-cookie", "s=1")]
                ),
                None,
            ),
            Err(Refusal::SetsACookie)
        );
    }

    #[test]
    fn an_authorised_request_is_private_unless_the_response_says_otherwise() {
        let authorised = request(http::Method::GET, &[("authorization", "Bearer x")]);
        assert_eq!(
            freshness(
                &authorised,
                &response(200, &[("cache-control", "max-age=600")]),
                None
            ),
            Err(Refusal::Authorised)
        );
        // `public` and `s-maxage` are the two that unambiguously permit it.
        assert!(
            freshness(
                &authorised,
                &response(200, &[("cache-control", "public, max-age=600")]),
                None
            )
            .is_ok()
        );
        assert!(
            freshness(
                &authorised,
                &response(200, &[("cache-control", "s-maxage=600")]),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn vary_star_means_nothing_can_be_matched_on() {
        assert_eq!(
            freshness(
                &plain(),
                &response(200, &[("cache-control", "max-age=600"), ("vary", "*")]),
                None
            ),
            Err(Refusal::VariesOnEverything)
        );
    }

    #[test]
    fn a_status_that_is_not_worth_keeping_is_not_kept() {
        for status in [201, 206, 302, 304, 500, 502, 503] {
            assert_eq!(
                freshness(
                    &plain(),
                    &response(status, &[("cache-control", "max-age=600")]),
                    None
                ),
                Err(Refusal::Status),
                "{status}"
            );
        }
    }

    #[test]
    fn max_age_zero_is_not_freshness() {
        assert_eq!(
            freshness(
                &plain(),
                &response(200, &[("cache-control", "max-age=0")]),
                Some(Duration::from_secs(60))
            ),
            Err(Refusal::NoFreshness),
            "a response saying it is already stale must not be revived by a default"
        );
    }

    #[test]
    fn expires_is_measured_against_the_responses_own_date() {
        // Against `Date` rather than the local clock, so a gateway whose clock
        // is a minute out does not shorten every lifetime by a minute.
        let fresh = freshness(
            &plain(),
            &response(
                200,
                &[
                    ("date", "Sun, 06 Nov 1994 08:49:37 GMT"),
                    ("expires", "Sun, 06 Nov 1994 09:49:37 GMT"),
                ],
            ),
            None,
        );
        assert_eq!(fresh, Ok(Duration::from_secs(3600)));
    }

    #[test]
    fn an_expires_in_the_past_is_not_freshness() {
        assert_eq!(
            freshness(
                &plain(),
                &response(
                    200,
                    &[
                        ("date", "Sun, 06 Nov 1994 08:49:37 GMT"),
                        ("expires", "Sun, 06 Nov 1994 08:49:37 GMT"),
                    ],
                ),
                None,
            ),
            Err(Refusal::NoFreshness)
        );
    }

    #[test]
    fn a_client_can_refuse_the_cache() {
        assert_eq!(
            request_is_cacheable(&request(http::Method::GET, &[("cache-control", "no-store")])),
            Err(Refusal::ClientSaidNo)
        );
        assert!(!may_serve_from_cache(&request(
            http::Method::GET,
            &[("cache-control", "no-cache")]
        )));
        assert!(may_serve_from_cache(&plain()));
    }

    #[test]
    fn vary_names_are_normalised_so_one_order_is_one_key() {
        assert_eq!(
            vary_on(&response(200, &[("vary", "Accept-Encoding, Accept")])),
            vec!["accept".to_string(), "accept-encoding".to_string()]
        );
        assert!(vary_on(&response(200, &[])).is_empty());
    }

    #[test]
    fn http_dates_parse_or_are_ignored() {
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
        // A leap year, which an off-by-one in the month table would get wrong.
        assert_eq!(http_date("Tue, 29 Feb 2000 00:00:00 GMT"), Some(951782400));
        // The obsolete forms are not parsed, and must not be half-parsed.
        assert_eq!(http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(http_date("nonsense"), None);
    }
}
