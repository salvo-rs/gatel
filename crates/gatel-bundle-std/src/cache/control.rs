//! Conservative Cache-Control parsing across all field lines.
use http::header::CACHE_CONTROL;
use http::{HeaderMap, HeaderName};

pub(super) struct Control(Vec<(String, Option<String>)>);

impl Control {
    pub(super) fn parse(headers: &HeaderMap) -> Result<Self, ()> {
        let mut entries = Vec::new();
        for value in headers.get_all(CACHE_CONTROL) {
            let text = value.to_str().map_err(|_| ())?;
            let mut start = 0;
            let mut quoted = false;
            let mut escaped = false;
            for (index, byte) in text.bytes().chain(std::iter::once(b',')).enumerate() {
                if escaped {
                    escaped = false;
                } else if quoted && byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = !quoted;
                } else if byte == b',' && !quoted {
                    let entry = text[start..index].trim();
                    start = index + 1;
                    if entry.is_empty() {
                        continue;
                    }
                    let (name, value) = match entry.split_once('=') {
                        Some((name, value)) => (name.trim(), Some(parse_value(value.trim())?)),
                        None => (entry, None),
                    };
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| ())?;
                    let name = name.to_ascii_lowercase();
                    // Ambiguous duplicate directives never enable cache reuse.
                    if entries.iter().any(|(existing, _)| existing == &name) {
                        return Err(());
                    }
                    entries.push((name, value));
                }
            }
            if quoted || escaped {
                return Err(());
            }
        }
        let result = Self(entries);
        for name in ["max-age", "s-maxage", "min-fresh"] {
            result.seconds(name)?;
        }
        Ok(result)
    }

    pub(super) fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|(key, _)| key == name)
    }

    pub(super) fn seconds(&self, name: &str) -> Result<Option<u64>, ()> {
        let Some((_, value)) = self.0.iter().find(|(key, _)| key == name) else {
            return Ok(None);
        };
        let value = value.as_deref().ok_or(())?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        value.parse().map(Some).map_err(|_| ())
    }
}

fn parse_value(value: &str) -> Result<String, ()> {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        let mut decoded = String::new();
        let mut escaped = false;
        for c in inner.chars() {
            if escaped {
                decoded.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return Err(());
            } else {
                decoded.push(c);
            }
        }
        if escaped {
            return Err(());
        }
        Ok(decoded)
    } else {
        HeaderName::from_bytes(value.as_bytes()).map_err(|_| ())?;
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_lists_and_malformed_freshness() {
        let parse = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CACHE_CONTROL, http::HeaderValue::from_static(value));
            Control::parse(&headers)
        };
        let control = parse("private=\"x-user, x-role\", max-age=\"60\"").unwrap();
        assert!(control.contains("private"));
        assert_eq!(control.seconds("max-age"), Ok(Some(60)));
        assert!(
            !parse("extension=\"private, no-store\"")
                .unwrap()
                .contains("private")
        );
        for invalid in [
            "max-age",
            "max-age=-1",
            "max-age=+1",
            "max-age=999999999999999999999",
            "max-age=1, MAX-AGE=2",
            "max-age=\"60",
            "private=\"x\"junk",
            "max-age=60, s-maxage=bad",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
    }
}
