/// Path matching strategies and advanced request matchers.
use std::net::SocketAddr;

use http::Request;

use crate::Body;
use crate::glob::glob_matches;

// ---------------------------------------------------------------------------
// Path matching (existing functionality)
// ---------------------------------------------------------------------------

/// Match a request path against a route pattern.
///
/// Supported patterns:
/// - `"/*"` — matches everything
/// - `"/api/*"` — prefix match (path starts with `/api/`)
/// - `"/exact"` — exact match
/// - `"*.php"` — suffix/extension match
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/*" || pattern == "*" {
        return true;
    }
    // Extension glob: "*.php" matches any path ending with ".php"
    if pattern.starts_with('*') && !pattern.starts_with("**") {
        let suffix = &pattern[1..];
        return path.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // Prefix match: "/api/*" matches "/api/", "/api/foo", "/api/foo/bar"
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if pattern.ends_with('*') {
        // Glob-like: "/static*" matches "/static", "/staticfiles", etc.
        let prefix = &pattern[..pattern.len() - 1];
        return path.starts_with(prefix);
    }
    // Exact match
    path == pattern
}

/// Sort patterns by specificity (most specific first).
/// Longer non-wildcard prefixes are more specific.
pub fn pattern_specificity(pattern: &str) -> usize {
    if pattern == "/*" || pattern == "*" {
        return 0;
    }
    if pattern.starts_with('*') {
        // Extension matchers like "*.php" are somewhat specific.
        return pattern.len();
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return prefix.len() + 1;
    }
    if pattern.ends_with('*') {
        return pattern.len();
    }
    // Exact match is most specific
    pattern.len() + 1000
}

// ---------------------------------------------------------------------------
// Advanced request matchers
// ---------------------------------------------------------------------------

/// A composable request matcher that can test various aspects of an incoming
/// HTTP request.  Matchers can be combined with `And`, `Or`, and `Not`.
#[derive(Debug, Clone, serde::Serialize)]
pub enum RequestMatcher {
    /// Match the request path using glob-style patterns.
    Path(String),
    /// Match the HTTP method (e.g. `["GET", "POST"]`).
    Method(Vec<String>),
    /// Match a header value with glob pattern (e.g. `name="X-Custom"`, `pattern="foo*"`).
    Header { name: String, pattern: String },
    /// Match a header value with regex-like glob pattern.
    HeaderRegex { name: String, regex: String },
    /// Match a query parameter.  If `value` is `None`, just check presence.
    Query { key: String, value: Option<String> },
    /// Match client IP against CIDR ranges (e.g. `["192.168.0.0/16", "10.0.0.0/8"]`).
    RemoteIp(Vec<String>),
    /// Match the protocol/scheme (e.g. `"https"`, `"http"`).
    Protocol(String),
    /// Simple expression matcher: `"{method} == GET && {path} ~ /api/*"`.
    Expression(String),
    /// Logical NOT.
    Not(Box<RequestMatcher>),
    /// Logical AND: all must match.
    And(Vec<RequestMatcher>),
    /// Logical OR: at least one must match.
    Or(Vec<RequestMatcher>),
    /// Match the Accept-Language header against a list of language tags.
    /// Uses case-insensitive prefix matching (e.g. "en" matches "en-US").
    Language(Vec<String>),
}

impl RequestMatcher {
    /// Test whether an incoming request matches this matcher.
    pub fn matches(&self, req: &Request<Body>, client_addr: SocketAddr) -> bool {
        match self {
            RequestMatcher::Path(pattern) => {
                let path = req.uri().path();
                path_matches(pattern, path)
            }

            RequestMatcher::Method(methods) => {
                let req_method = req.method().as_str().to_uppercase();
                methods.iter().any(|m| m.to_uppercase() == req_method)
            }

            RequestMatcher::Header { name, pattern } => {
                if let Ok(header_name) = name.parse::<http::header::HeaderName>() {
                    req.headers()
                        .get(&header_name)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| glob_matches(pattern, v))
                        .unwrap_or(false)
                } else {
                    false
                }
            }

            RequestMatcher::HeaderRegex { name, regex } => {
                if let Ok(header_name) = name.parse::<http::header::HeaderName>() {
                    req.headers()
                        .get(&header_name)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| glob_matches(regex, v))
                        .unwrap_or(false)
                } else {
                    false
                }
            }

            RequestMatcher::Query { key, value } => {
                let query_str = req.uri().query().unwrap_or("");
                match_query_param(query_str, key, value.as_deref())
            }

            RequestMatcher::RemoteIp(cidrs) => {
                let client_ip = client_addr.ip();
                cidrs.iter().any(|cidr| match_cidr(cidr, &client_ip))
            }

            RequestMatcher::Protocol(proto) => {
                let scheme = req.uri().scheme_str().unwrap_or("http");
                scheme.eq_ignore_ascii_case(proto)
            }

            RequestMatcher::Expression(expr) => eval_expression(expr, req, client_addr),

            RequestMatcher::Not(inner) => !inner.matches(req, client_addr),

            RequestMatcher::And(matchers) => matchers.iter().all(|m| m.matches(req, client_addr)),

            RequestMatcher::Or(matchers) => matchers.iter().any(|m| m.matches(req, client_addr)),

            RequestMatcher::Language(langs) => {
                // Parse Accept-Language header value and check for prefix matches.
                // E.g. "en-US,en;q=0.9,fr;q=0.8" → ["en-US", "en", "fr"]
                let header_value = req
                    .headers()
                    .get(http::header::ACCEPT_LANGUAGE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                // Extract the language tags (strip quality values).
                let accepted: Vec<&str> = header_value
                    .split(',')
                    .map(|part| part.split(';').next().unwrap_or("").trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                langs.iter().any(|configured| {
                    accepted.iter().any(|accepted_lang| {
                        // Prefix match: "en" matches "en-US" or "en"
                        let c = configured.to_lowercase();
                        let a = accepted_lang.to_lowercase();
                        a == c || a.starts_with(&format!("{c}-"))
                    })
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Glob matching
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Query parameter matching
// ---------------------------------------------------------------------------

/// Check if a query string contains a parameter with the given key
/// (and optionally value).
fn match_query_param(query: &str, key: &str, value: Option<&str>) -> bool {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = if let Some(eq_pos) = pair.find('=') {
            (&pair[..eq_pos], Some(&pair[eq_pos + 1..]))
        } else {
            (pair, None)
        };
        if k == key {
            match value {
                None => return true, // just check presence
                Some(expected) => {
                    if v == Some(expected) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// CIDR matching
// ---------------------------------------------------------------------------

/// Public re-export of CIDR matching used by the router condition evaluator.
pub fn match_cidr_pub(cidr: &str, ip: &std::net::IpAddr) -> bool {
    match_cidr(cidr, ip)
}

/// Match an IP address against a CIDR range.
///
/// Supports:
/// - Exact IP: `"192.168.1.1"`
/// - CIDR notation: `"192.168.0.0/16"`, `"10.0.0.0/8"`
/// - IPv6: `"::1"`, `"fd00::/8"`
fn match_cidr(cidr: &str, ip: &std::net::IpAddr) -> bool {
    if let Some(slash_pos) = cidr.find('/') {
        let network_str = &cidr[..slash_pos];
        let prefix_str = &cidr[slash_pos + 1..];

        let network: std::net::IpAddr = match network_str.parse() {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        let prefix_len: u32 = match prefix_str.parse() {
            Ok(p) => p,
            Err(_) => return false,
        };

        match (network, ip) {
            (std::net::IpAddr::V4(net), std::net::IpAddr::V4(addr)) => {
                if prefix_len > 32 {
                    return false;
                }
                if prefix_len == 0 {
                    return true;
                }
                let mask = u32::MAX << (32 - prefix_len);
                (u32::from(*addr) & mask) == (u32::from(net) & mask)
            }
            (std::net::IpAddr::V6(net), std::net::IpAddr::V6(addr)) => {
                if prefix_len > 128 {
                    return false;
                }
                if prefix_len == 0 {
                    return true;
                }
                let net_bits = u128::from(net);
                let addr_bits = u128::from(*addr);
                let mask = u128::MAX << (128 - prefix_len);
                (addr_bits & mask) == (net_bits & mask)
            }
            _ => false, // v4/v6 mismatch
        }
    } else {
        // Exact IP match.
        match cidr.parse::<std::net::IpAddr>() {
            Ok(expected) => *ip == expected,
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Simple expression evaluation
// ---------------------------------------------------------------------------

/// Evaluate a simple expression against a request.
///
/// Supported tokens:
/// - `{method}` — HTTP method
/// - `{path}` — request path
/// - `{host}` — Host header
/// - `{remote_ip}` — client IP
///
/// Operators:
/// - `==` — exact equality
/// - `!=` — inequality
/// - `~` — glob match
///
/// Combinators:
/// - `&&` — logical AND
/// - `||` — logical OR
fn eval_expression(expr: &str, req: &Request<Body>, client_addr: SocketAddr) -> bool {
    // Split by "||" first (lowest precedence), then "&&".
    let or_parts: Vec<&str> = expr.split("||").collect();
    for or_part in &or_parts {
        let and_parts: Vec<&str> = or_part.split("&&").collect();
        let all_match = and_parts
            .iter()
            .all(|part| eval_single_condition(part.trim(), req, client_addr));
        if all_match {
            return true;
        }
    }
    false
}

fn eval_single_condition(cond: &str, req: &Request<Body>, client_addr: SocketAddr) -> bool {
    // Try to parse: "{var} op value"
    let (var, op, value) = if let Some(pos) = cond.find("!=") {
        let var = cond[..pos].trim();
        let value = cond[pos + 2..].trim();
        (var, "!=", value)
    } else if let Some(pos) = cond.find("==") {
        let var = cond[..pos].trim();
        let value = cond[pos + 2..].trim();
        (var, "==", value)
    } else if let Some(pos) = cond.find('~') {
        let var = cond[..pos].trim();
        let value = cond[pos + 1..].trim();
        (var, "~", value)
    } else {
        // Cannot parse; treat as false.
        return false;
    };

    let resolved = resolve_variable(var, req, client_addr);

    match op {
        "==" => resolved == value,
        "!=" => resolved != value,
        "~" => glob_matches(value, &resolved),
        _ => false,
    }
}

fn resolve_variable(var: &str, req: &Request<Body>, client_addr: SocketAddr) -> String {
    match var.trim_matches(|c| c == '{' || c == '}') {
        "method" => req.method().to_string(),
        "path" => req.uri().path().to_string(),
        "host" => req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
        "remote_ip" => client_addr.ip().to_string(),
        "scheme" | "protocol" => req.uri().scheme_str().unwrap_or("http").to_string(),
        "query" => req.uri().query().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wildcard() {
        assert!(path_matches("/*", "/anything"));
        assert!(path_matches("/*", "/"));
        assert!(path_matches("*", "/foo"));
    }

    #[test]
    fn test_prefix() {
        assert!(path_matches("/api/*", "/api/users"));
        assert!(path_matches("/api/*", "/api/"));
        assert!(path_matches("/api/*", "/api"));
        assert!(!path_matches("/api/*", "/apifoo"));
        assert!(!path_matches("/api/*", "/other"));
    }

    #[test]
    fn test_exact() {
        assert!(path_matches("/health", "/health"));
        assert!(!path_matches("/health", "/health/check"));
    }

    #[test]
    fn test_extension_match() {
        assert!(path_matches("*.php", "/index.php"));
        assert!(path_matches("*.php", "/app/page.php"));
        assert!(!path_matches("*.php", "/index.html"));
    }

    #[test]
    fn test_specificity_ordering() {
        assert!(pattern_specificity("/api/*") > pattern_specificity("/*"));
        assert!(pattern_specificity("/api/v1/*") > pattern_specificity("/api/*"));
        assert!(pattern_specificity("/exact") > pattern_specificity("/api/v1/*"));
    }

    #[test]
    fn test_glob_matches_star() {
        assert!(glob_matches("foo*", "foobar"));
        assert!(glob_matches("foo*", "foo"));
        assert!(!glob_matches("foo*", "baz"));
        assert!(glob_matches("*bar", "foobar"));
        assert!(!glob_matches("foo*", "foo/bar"));
    }

    #[test]
    fn test_glob_matches_double_star() {
        assert!(glob_matches("**", "anything/at/all"));
        assert!(glob_matches("/api/**", "/api/v1/users"));
        assert!(glob_matches("foo/**/bar", "foo/a/b/c/bar"));
    }

    #[test]
    fn test_glob_matches_question() {
        assert!(glob_matches("fo?", "foo"));
        assert!(glob_matches("fo?", "fob"));
        assert!(!glob_matches("fo?", "fooo"));
    }

    #[test]
    fn test_query_param() {
        assert!(match_query_param("a=1&b=2", "a", Some("1")));
        assert!(match_query_param("a=1&b=2", "b", None));
        assert!(!match_query_param("a=1&b=2", "c", None));
        assert!(!match_query_param("a=1", "a", Some("2")));
    }

    #[test]
    fn test_cidr_match_v4() {
        let ip: std::net::IpAddr = "192.168.1.100".parse().unwrap();
        assert!(match_cidr("192.168.0.0/16", &ip));
        assert!(match_cidr("192.168.1.0/24", &ip));
        assert!(!match_cidr("10.0.0.0/8", &ip));
        assert!(match_cidr("192.168.1.100", &ip));
    }

    #[test]
    fn test_cidr_match_v6() {
        let ip: std::net::IpAddr = "::1".parse().unwrap();
        assert!(match_cidr("::1", &ip));
        assert!(match_cidr("::0/0", &ip));
    }

    #[test]
    fn test_request_matcher_method() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/test")
            .body(crate::empty_body())
            .unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let matcher = RequestMatcher::Method(vec!["GET".into(), "POST".into()]);
        assert!(matcher.matches(&req, addr));

        let matcher = RequestMatcher::Method(vec!["POST".into()]);
        assert!(!matcher.matches(&req, addr));
    }

    #[test]
    fn test_request_matcher_query() {
        let req = http::Request::builder()
            .uri("/test?foo=bar&baz=1")
            .body(crate::empty_body())
            .unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let matcher = RequestMatcher::Query {
            key: "foo".into(),
            value: Some("bar".into()),
        };
        assert!(matcher.matches(&req, addr));

        let matcher = RequestMatcher::Query {
            key: "baz".into(),
            value: None,
        };
        assert!(matcher.matches(&req, addr));

        let matcher = RequestMatcher::Query {
            key: "missing".into(),
            value: None,
        };
        assert!(!matcher.matches(&req, addr));
    }

    #[test]
    fn test_request_matcher_not() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/test")
            .body(crate::empty_body())
            .unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let matcher = RequestMatcher::Not(Box::new(RequestMatcher::Method(vec!["POST".into()])));
        assert!(matcher.matches(&req, addr));
    }

    #[test]
    fn test_request_matcher_and_or() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/api/test?debug=1")
            .body(crate::empty_body())
            .unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let matcher = RequestMatcher::And(vec![
            RequestMatcher::Method(vec!["GET".into()]),
            RequestMatcher::Path("/api/*".into()),
        ]);
        assert!(matcher.matches(&req, addr));

        let matcher = RequestMatcher::Or(vec![
            RequestMatcher::Method(vec!["POST".into()]),
            RequestMatcher::Path("/api/*".into()),
        ]);
        assert!(matcher.matches(&req, addr));
    }

    #[test]
    fn test_request_matcher_remote_ip() {
        let req = http::Request::builder()
            .uri("/test")
            .body(crate::empty_body())
            .unwrap();
        let addr: SocketAddr = "192.168.1.50:1234".parse().unwrap();

        let matcher = RequestMatcher::RemoteIp(vec!["192.168.0.0/16".into()]);
        assert!(matcher.matches(&req, addr));

        let matcher = RequestMatcher::RemoteIp(vec!["10.0.0.0/8".into()]);
        assert!(!matcher.matches(&req, addr));
    }
}
