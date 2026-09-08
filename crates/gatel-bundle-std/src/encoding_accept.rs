//! Strict Accept-Encoding preferences shared by representation selectors.
use http::{HeaderMap, header};

/// Keep identity preference distinct from an explicit refusal of every offer.
/// Callers must decide rejection at the final representation boundary: a file
/// server cannot reject an identity intermediate before an outer encoder runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection<'a> {
    Coding(&'a str),
    Identity,
    NotAcceptable,
    /// Preserve the conservative no-transformation policy for malformed input,
    /// without pretending it was a valid affirmative identity preference.
    Invalid,
}

impl<'a> Selection<'a> {
    /// Legacy consumers only transform on an affirmative coding selection.
    /// This does not mean identity was accepted for every other result.
    pub(crate) fn coding(self) -> Option<&'a str> {
        match self {
            Self::Coding(coding) => Some(coding),
            _ => None,
        }
    }
}

/// Client quality wins; server order breaks ties. Missing/empty fields select
/// identity; malformed fields are distinguished from valid negotiation.
pub(crate) fn select<'a>(headers: &HeaderMap, supported: impl IntoIterator<Item = &'a str>) -> Selection<'a> {
    let Some(offers) = parse(headers) else {
        return Selection::Invalid;
    };
    let wildcard = offers.get("*").copied().unwrap_or(0);
    let mut best = None;
    let mut best_quality = 0;
    for coding in supported {
        let quality = offers.get(coding).copied().unwrap_or(wildcard);
        if quality > best_quality {
            best = Some(coding);
            best_quality = quality;
        }
    }
    if offers
        .get("identity")
        .is_some_and(|quality| *quality > best_quality)
    {
        return Selection::Identity;
    }
    if let Some(coding) = best {
        return Selection::Coding(coding);
    }
    // RFC 9110 §12.5.3: identity is acceptable by default; only explicit zero
    // or a wildcard zero without a specific identity entry excludes it.
    if offers.get("identity").or_else(|| offers.get("*")) == Some(&0) {
        Selection::NotAcceptable
    } else {
        Selection::Identity
    }
}

fn parse(headers: &HeaderMap) -> Option<std::collections::BTreeMap<String, u16>> {
    let mut offers = std::collections::BTreeMap::<String, u16>::new();
    for field in headers.get_all(header::ACCEPT_ENCODING) {
        for item in field.to_str().ok()?.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let mut parts = item.split(';');
            let coding = parts.next()?.trim();
            // HeaderName uses the same HTTP token grammar, including '*'.
            http::HeaderName::from_bytes(coding.as_bytes()).ok()?;
            let quality = match parts.next() {
                None => 1000,
                Some(parameter) => {
                    let (name, value) = parameter.trim().split_once('=')?;
                    if !name.trim().eq_ignore_ascii_case("q") {
                        return None;
                    }
                    quality(value.trim())?
                }
            };
            if parts.next().is_some() {
                return None;
            }
            // Contradictory duplicates must never override an explicit refusal.
            offers
                .entry(coding.to_ascii_lowercase())
                .and_modify(|existing| *existing = (*existing).min(quality))
                .or_insert(quality);
        }
    }
    Some(offers)
}

fn quality(raw: &str) -> Option<u16> {
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if !matches!(whole, "0" | "1") || fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if whole == "1" {
        return fraction.bytes().all(|b| b == b'0').then_some(1000);
    }
    let mut value = 0;
    for (index, digit) in fraction.bytes().enumerate() {
        value += u16::from(digit - b'0') * [100, 10, 1][index];
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn choose(fields: &[&str]) -> Option<&'static str> {
        let mut headers = HeaderMap::new();
        for field in fields {
            headers.append(header::ACCEPT_ENCODING, field.parse().unwrap());
        }
        select(&headers, ["br", "gzip"]).coding()
    }

    #[test]
    fn explicit_refusal_overrides_wildcard_across_field_lines() {
        assert_eq!(choose(&["br;q=0, gzip;q=0", "*;q=1"]), None);
        assert_eq!(choose(&["br;q=0", "*"]), Some("gzip"));
        assert_eq!(choose(&["*;q=0", "GZIP;Q=1.000"]), Some("gzip"));
    }

    #[test]
    fn quality_wins_and_server_order_breaks_ties() {
        assert_eq!(choose(&["br;q=0.5, gzip;q=0.9"]), Some("gzip"));
        assert_eq!(choose(&["gzip, br"]), Some("br"));
        assert_eq!(choose(&["gzip;q=0.001"]), Some("gzip"));
        assert_eq!(choose(&["gzip;q=0."]), None);
        assert_eq!(choose(&["gzip;q=1."]), Some("gzip"));
        assert_eq!(choose(&["gzip;q=0.5, identity;q=1"]), None);
    }

    #[test]
    fn duplicate_offers_are_conservative_and_order_independent() {
        assert_eq!(choose(&["gzip;q=0", "gzip"]), None);
        assert_eq!(choose(&["gzip", "gzip;q=0"]), None);
    }

    #[test]
    fn malformed_or_absent_preferences_do_not_enable_compression() {
        for value in [
            "gzip;q=NaN",
            "gzip;q=inf",
            "gzip;q=2",
            "gzip;q=-1",
            "gzip;q=.5",
            "gzip;q=0.1234",
            "gzip;q=1.001",
            "gzip;q=1;q=0",
            "gzip;level=1",
            "gzip;q=",
            "gzip;q=0.5.0",
            "bad token, gzip",
        ] {
            assert_eq!(choose(&[value]), None, "{value}");
        }
        assert_eq!(choose(&[]), None);
        assert_eq!(choose(&["", ", ,"]), None);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            http::HeaderValue::from_bytes(b"gzip,\xff").unwrap(),
        );
        assert_eq!(select(&headers, ["gzip"]), Selection::Invalid);
    }

    #[test]
    fn identity_fallback_and_exhausted_offers_are_distinct() {
        use Selection::*;
        for (fields, expected) in [
            (vec![], Identity),
            (vec![""], Identity),
            (vec!["gzip;q=0"], Identity),
            (vec!["*;q=0"], NotAcceptable),
            (vec!["identity;q=0"], NotAcceptable),
            (vec!["gzip;q=0, identity;q=0"], NotAcceptable),
            (vec!["*;q=0, identity;q=0.1"], Identity),
            (vec!["*;q=1, identity;q=0"], Coding("gzip")),
            (vec!["identity;q=1, gzip;q=0.5"], Identity),
            (vec!["identity;q=0", "identity;q=1"], NotAcceptable),
            (vec!["identity;q=0", "gzip;q=1"], Coding("gzip")),
            (vec!["identity;q=0, gzip;q=broken"], Invalid),
        ] {
            let mut headers = HeaderMap::new();
            for field in &fields {
                headers.append(header::ACCEPT_ENCODING, field.parse().unwrap());
            }
            assert_eq!(select(&headers, ["gzip"]), expected, "{fields:?}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "gzip, identity;q=0".parse().unwrap());
        assert_eq!(select(&headers, []), NotAcceptable);
    }
}
