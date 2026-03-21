//! Cache-Control directive parsing and Vary key building.

use std::time::Duration;

use http::HeaderMap;

/// Parsed Cache-Control directives.
#[derive(Debug, Clone, Default)]
pub struct CacheControlDirectives {
    pub no_store: bool,
    pub no_cache: bool,
    pub max_age: Option<Duration>,
    pub s_maxage: Option<Duration>,
}

/// Parse a `Cache-Control` header value into its directives.
pub fn parse_cache_control(value: &str) -> CacheControlDirectives {
    let mut directives = CacheControlDirectives::default();

    for part in value.split(',') {
        let part = part.trim().to_ascii_lowercase();
        if part == "no-store" {
            directives.no_store = true;
        } else if part == "no-cache" {
            directives.no_cache = true;
        } else if let Some(value) = part.strip_prefix("max-age=") {
            if let Ok(seconds) = value.trim().parse::<u64>() {
                directives.max_age = Some(Duration::from_secs(seconds));
            }
        } else if let Some(value) = part.strip_prefix("s-maxage=")
            && let Ok(seconds) = value.trim().parse::<u64>()
        {
            directives.s_maxage = Some(Duration::from_secs(seconds));
        }
    }

    directives
}

/// Build a vary key from the `Vary` response header and the request headers.
pub fn build_vary_key(response_headers: &HeaderMap, request_headers: &HeaderMap) -> String {
    let vary = match response_headers.get(http::header::VARY) {
        Some(value) => match value.to_str() {
            Ok(value) => value,
            Err(_) => return String::new(),
        },
        None => return String::new(),
    };

    if vary == "*" {
        return "*".to_string();
    }

    let mut parts = Vec::new();
    for field_name in vary.split(',') {
        let field_name = field_name.trim().to_ascii_lowercase();
        let value = request_headers
            .get(field_name.as_str())
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        parts.push(format!("{field_name}={value}"));
    }
    parts.sort();
    parts.join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_no_store() {
        let directives = parse_cache_control("no-store");
        assert!(directives.no_store);
        assert!(!directives.no_cache);
    }

    #[test]
    fn parse_no_cache() {
        let directives = parse_cache_control("no-cache");
        assert!(directives.no_cache);
        assert!(!directives.no_store);
    }

    #[test]
    fn parse_max_age() {
        let directives = parse_cache_control("max-age=3600");
        assert_eq!(directives.max_age, Some(Duration::from_secs(3600)));
        assert!(directives.s_maxage.is_none());
    }

    #[test]
    fn build_vary_key_with_values() {
        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            http::header::VARY,
            "Accept-Encoding, Accept-Language".parse().unwrap(),
        );
        let mut request_headers = HeaderMap::new();
        request_headers.insert(http::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
        request_headers.insert(http::header::ACCEPT_LANGUAGE, "en-US".parse().unwrap());
        let key = build_vary_key(&response_headers, &request_headers);
        assert!(key.contains("accept-encoding=gzip"));
        assert!(key.contains("accept-language=en-US"));
    }
}
