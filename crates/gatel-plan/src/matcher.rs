//! Request matching.
//!
//! Matchers are pure data plus a decision function over a small context, so
//! routing can be tested exhaustively without a socket, and so the same types
//! describe a route in `gatel explain` and select it at request time.

use std::cmp::Ordering;
use std::net::IpAddr;

use gatel_model::{Code, Diagnostics, Hasher, Node, Origin};

/// Everything routing is allowed to look at. Deliberately small: anything not
/// here cannot influence which route is chosen.
pub struct MatchContext<'a> {
    pub host: &'a str,
    pub method: &'a str,
    /// Percent-encoded path, without the query string.
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: &'a dyn HeaderLookup,
    pub client_ip: Option<IpAddr>,
    pub tls: bool,
}

pub trait HeaderLookup {
    fn get(&self, name: &str) -> Option<&str>;
    fn cookie(&self, name: &str) -> Option<&str>;
}

/// The host a site claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMatch {
    /// `site "*"`, the fallback for any host.
    Any,
    /// A lowercase DNS name without a port.
    Exact(String),
    /// `*.example.com`: matches one or more leading labels, never the bare
    /// parent domain.
    Suffix(String),
}

impl HostMatch {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed == "*" {
            return Ok(HostMatch::Any);
        }
        if trimmed.is_empty() {
            return Err("a site host may not be empty".to_string());
        }
        if trimmed.contains(':') {
            return Err(format!("`{trimmed}` must not contain a port"));
        }
        let lowered = trimmed.to_ascii_lowercase();
        if let Some(rest) = lowered.strip_prefix("*.") {
            if rest.is_empty() || rest.contains('*') {
                return Err(format!("`{raw}` is not a valid wildcard host"));
            }
            return Ok(HostMatch::Suffix(format!(".{rest}")));
        }
        if lowered.contains('*') {
            return Err(format!("`{raw}` may only use `*` as a leading label"));
        }
        Ok(HostMatch::Exact(lowered))
    }

    pub fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Any => true,
            HostMatch::Exact(expected) => host.eq_ignore_ascii_case(expected),
            HostMatch::Suffix(suffix) => {
                host.len() > suffix.len() && host.to_ascii_lowercase().ends_with(suffix)
            }
        }
    }

    /// Higher wins. Exact beats wildcard beats catch-all.
    pub fn specificity(&self) -> (u8, usize) {
        match self {
            HostMatch::Exact(name) => (2, name.len()),
            HostMatch::Suffix(suffix) => (1, suffix.len()),
            HostMatch::Any => (0, 0),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            HostMatch::Any => "*".to_string(),
            HostMatch::Exact(name) => name.clone(),
            HostMatch::Suffix(suffix) => format!("*{suffix}"),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

/// The path a route claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathMatch {
    Exact(String),
    /// From `/api/*`. Stores `/api/`, and also matches the bare `/api`.
    Prefix(String),
}

impl PathMatch {
    pub fn parse(raw: &str) -> Result<Self, String> {
        if !raw.starts_with('/') {
            return Err(format!("`{raw}` must start with `/`"));
        }
        if raw.contains('?') || raw.contains('#') {
            return Err(format!("`{raw}` must not contain a query or fragment"));
        }
        match raw.strip_suffix('*') {
            Some(prefix) => {
                if prefix.contains('*') {
                    return Err(format!("`{raw}` may only use `*` at the end"));
                }
                Ok(PathMatch::Prefix(prefix.to_string()))
            }
            None => {
                if raw.contains('*') {
                    return Err(format!("`{raw}` may only use `*` at the end"));
                }
                Ok(PathMatch::Exact(raw.to_string()))
            }
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        match self {
            PathMatch::Exact(expected) => path == expected,
            PathMatch::Prefix(prefix) => {
                if path.starts_with(prefix) {
                    return true;
                }
                // `/api/*` should also answer `/api`, which is what an operator
                // means and what every other proxy does.
                prefix.strip_suffix('/').is_some_and(|bare| path == bare)
            }
        }
    }

    /// Higher wins: exact beats any prefix, longer prefix beats shorter.
    pub fn specificity(&self) -> (u8, usize) {
        match self {
            PathMatch::Exact(path) => (1, path.len()),
            PathMatch::Prefix(prefix) => (0, prefix.len()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            PathMatch::Exact(path) => path.clone(),
            PathMatch::Prefix(prefix) => format!("{prefix}*"),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

/// The methods a route accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodMatch {
    Any,
    /// Uppercase, sorted and deduplicated so declaration order is irrelevant.
    Only(Vec<String>),
}

impl MethodMatch {
    pub fn parse(raw: &[String]) -> Result<Self, String> {
        if raw.iter().any(|method| method == "*") {
            if raw.len() > 1 {
                return Err("`*` cannot be combined with named methods".to_string());
            }
            return Ok(MethodMatch::Any);
        }
        let mut methods = Vec::new();
        for method in raw {
            if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-') {
                return Err(format!("`{method}` is not an HTTP method"));
            }
            let upper = method.to_ascii_uppercase();
            if !methods.contains(&upper) {
                methods.push(upper);
            }
        }
        if methods.is_empty() {
            return Ok(MethodMatch::Any);
        }
        methods.sort();
        Ok(MethodMatch::Only(methods))
    }

    pub fn matches(&self, method: &str) -> bool {
        match self {
            MethodMatch::Any => true,
            MethodMatch::Only(methods) => {
                methods.iter().any(|candidate| candidate == method)
                    // A route that serves GET must answer HEAD, or every
                    // health checker and cache probe breaks.
                    || (method == "HEAD" && methods.iter().any(|candidate| candidate == "GET"))
            }
        }
    }

    pub fn specificity(&self) -> u8 {
        match self {
            MethodMatch::Only(_) => 1,
            MethodMatch::Any => 0,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            MethodMatch::Any => "*".to_string(),
            MethodMatch::Only(methods) => methods.join(" "),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

/// An additional condition beyond host, path and method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    Header {
        name: String,
        value: Option<String>,
    },
    Query {
        name: String,
        value: Option<String>,
    },
    Cookie {
        name: String,
        value: Option<String>,
    },
    /// A CIDR block against the canonical client address.
    ClientIp {
        network: IpAddr,
        prefix: u8,
    },
    Tls(bool),
    Not(Box<Matcher>),
}

impl Matcher {
    pub fn matches(&self, context: &MatchContext<'_>) -> bool {
        match self {
            Matcher::Header { name, value } => match_value(context.headers.get(name), value.as_deref()),
            Matcher::Cookie { name, value } => match_value(context.headers.cookie(name), value.as_deref()),
            Matcher::Query { name, value } => {
                match_value(query_value(context.query, name).as_deref(), value.as_deref())
            }
            Matcher::ClientIp { network, prefix } => context
                .client_ip
                .is_some_and(|ip| ip_in_network(ip, *network, *prefix)),
            Matcher::Tls(expected) => context.tls == *expected,
            Matcher::Not(inner) => !inner.matches(context),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Matcher::Header { name, value } => describe_pair("header", name, value.as_deref()),
            Matcher::Query { name, value } => describe_pair("query", name, value.as_deref()),
            Matcher::Cookie { name, value } => describe_pair("cookie", name, value.as_deref()),
            Matcher::ClientIp { network, prefix } => format!("client-ip {network}/{prefix}"),
            Matcher::Tls(expected) => format!("tls {expected}"),
            Matcher::Not(inner) => format!("not({})", inner.describe()),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

fn describe_pair(kind: &str, name: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{kind} {name}={value}"),
        None => format!("{kind} {name}"),
    }
}

fn match_value(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => actual == expected,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| value.to_string())
    })
}

fn ip_in_network(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    fn masked(bytes: &[u8], prefix: u8) -> Vec<u8> {
        let mut out = bytes.to_vec();
        let full = (prefix / 8) as usize;
        for (index, byte) in out.iter_mut().enumerate() {
            if index < full {
                continue;
            }
            if index == full {
                let remainder = prefix % 8;
                *byte &= if remainder == 0 {
                    0
                } else {
                    0xffu8 << (8 - remainder)
                };
            } else {
                *byte = 0;
            }
        }
        out
    }
    match (ip, network) {
        (IpAddr::V4(a), IpAddr::V4(b)) if prefix <= 32 => {
            masked(&a.octets(), prefix) == masked(&b.octets(), prefix)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) if prefix <= 128 => {
            masked(&a.octets(), prefix) == masked(&b.octets(), prefix)
        }
        _ => false,
    }
}

/// Parse `10.0.0.0/8` or a bare address, which means a single host.
pub fn parse_cidr(raw: &str) -> Result<(IpAddr, u8), String> {
    let (address, prefix) = match raw.split_once('/') {
        Some((address, prefix)) => (
            address,
            prefix
                .parse::<u8>()
                .map_err(|_| format!("`{prefix}` is not a prefix length"))?,
        ),
        None => (raw, u8::MAX),
    };
    let ip: IpAddr = address
        .parse()
        .map_err(|_| format!("`{address}` is not an IP address"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = if prefix == u8::MAX { max } else { prefix };
    if prefix > max {
        return Err(format!("/{prefix} is too long for {ip}"));
    }
    Ok((ip, prefix))
}

/// Decode a `match` node into extra conditions.
pub fn parse_match_node(node: &Node, diagnostics: &mut Diagnostics) -> Vec<Matcher> {
    let mut matchers = Vec::new();
    fn report(diagnostics: &mut Diagnostics, origin: &Origin, message: String) {
        diagnostics.error(Code::InvalidValue, origin.clone(), message);
    }

    for property in &node.props {
        let Some(text) = property
            .value
            .as_str()
            .map(str::to_string)
            .or_else(|| property.value.as_bool().map(|flag| flag.to_string()))
        else {
            report(
                diagnostics,
                &property.origin,
                format!("`{}` needs a string value", property.name),
            );
            continue;
        };
        match property.name.as_str() {
            "header" | "query" | "cookie" => {
                let (name, value) = match text.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (text.clone(), None),
                };
                if name.is_empty() {
                    report(
                        diagnostics,
                        &property.origin,
                        format!("`{}` needs a name", property.name),
                    );
                    continue;
                }
                matchers.push(match property.name.as_str() {
                    "header" => Matcher::Header {
                        name: name.to_ascii_lowercase(),
                        value,
                    },
                    "query" => Matcher::Query { name, value },
                    _ => Matcher::Cookie { name, value },
                });
            }
            "client-ip" => match parse_cidr(&text) {
                Ok((network, prefix)) => matchers.push(Matcher::ClientIp { network, prefix }),
                Err(message) => report(diagnostics, &property.origin, message),
            },
            "tls" => match property.value.as_bool() {
                Some(flag) => matchers.push(Matcher::Tls(flag)),
                None => report(
                    diagnostics,
                    &property.origin,
                    "`tls` must be `#true` or `#false`".to_string(),
                ),
            },
            other => {
                let known = ["header", "query", "cookie", "client-ip", "tls"];
                let mut diagnostic = gatel_model::Diagnostic::error(
                    Code::UnknownProperty,
                    property.origin.clone(),
                    format!("`match` has no condition `{other}`"),
                );
                if let Some(suggestion) = gatel_model::did_you_mean(other, known) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                }
                diagnostics.push(diagnostic);
            }
        }
    }
    if !node.args.is_empty() {
        diagnostics.error(
            Code::ArgumentCount,
            node.args[0].origin.clone(),
            "`match` takes conditions as properties, not positional arguments",
        );
    }
    matchers.sort_by_key(|a| a.describe());
    matchers
}

/// Route selection order: more specific first. Ties are impossible because
/// duplicate matchers are rejected at compile time.
pub fn compare_specificity(
    a: (&HostMatch, &PathMatch, &MethodMatch, usize),
    b: (&HostMatch, &PathMatch, &MethodMatch, usize),
) -> Ordering {
    b.0.specificity()
        .cmp(&a.0.specificity())
        .then_with(|| b.1.specificity().cmp(&a.1.specificity()))
        .then_with(|| b.2.specificity().cmp(&a.2.specificity()))
        .then_with(|| b.3.cmp(&a.3))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct Headers(HashMap<String, String>);

    impl HeaderLookup for Headers {
        fn get(&self, name: &str) -> Option<&str> {
            self.0.get(name).map(String::as_str)
        }

        fn cookie(&self, name: &str) -> Option<&str> {
            self.0.get(&format!("cookie:{name}")).map(String::as_str)
        }
    }

    fn context<'a>(path: &'a str, headers: &'a Headers) -> MatchContext<'a> {
        MatchContext {
            host: "example.com",
            method: "GET",
            path,
            query: Some("a=1&b=2"),
            headers,
            client_ip: Some("10.1.2.3".parse().unwrap()),
            tls: true,
        }
    }

    #[test]
    fn host_wildcards_never_match_the_bare_parent() {
        let wildcard = HostMatch::parse("*.example.com").unwrap();
        assert!(wildcard.matches("api.example.com"));
        assert!(wildcard.matches("a.b.example.com"));
        assert!(!wildcard.matches("example.com"));
        assert!(HostMatch::parse("*").unwrap().matches("anything"));
    }

    #[test]
    fn host_parsing_rejects_ports_and_interior_wildcards() {
        assert!(HostMatch::parse("example.com:443").is_err());
        assert!(HostMatch::parse("a.*.com").is_err());
        assert_eq!(
            HostMatch::parse("Example.COM").unwrap(),
            HostMatch::Exact("example.com".into())
        );
    }

    #[test]
    fn a_prefix_route_also_answers_its_bare_path() {
        let prefix = PathMatch::parse("/api/*").unwrap();
        assert!(prefix.matches("/api/users"));
        assert!(prefix.matches("/api/"));
        assert!(prefix.matches("/api"));
        assert!(!prefix.matches("/apifoo"));
    }

    #[test]
    fn exact_paths_beat_longer_prefixes() {
        let exact = PathMatch::parse("/a").unwrap();
        let long_prefix = PathMatch::parse("/a/very/long/*").unwrap();
        assert!(exact.specificity() > long_prefix.specificity());
    }

    #[test]
    fn get_routes_answer_head() {
        let methods = MethodMatch::parse(&["GET".to_string()]).unwrap();
        assert!(methods.matches("GET"));
        assert!(methods.matches("HEAD"));
        assert!(!methods.matches("POST"));
    }

    #[test]
    fn methods_normalize_so_declaration_order_is_irrelevant() {
        let a = MethodMatch::parse(&["post".into(), "GET".into()]).unwrap();
        let b = MethodMatch::parse(&["GET".into(), "POST".into(), "get".into()]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.describe(), "GET POST");
    }

    #[test]
    fn header_and_cookie_matchers_compare_presence_or_value() {
        let mut map = HashMap::new();
        map.insert("x-canary".to_string(), "1".to_string());
        map.insert("cookie:session".to_string(), "abc".to_string());
        let headers = Headers(map);
        let context = context("/", &headers);

        assert!(
            Matcher::Header {
                name: "x-canary".into(),
                value: None
            }
            .matches(&context)
        );
        assert!(
            Matcher::Header {
                name: "x-canary".into(),
                value: Some("1".into())
            }
            .matches(&context)
        );
        assert!(
            !Matcher::Header {
                name: "x-canary".into(),
                value: Some("2".into())
            }
            .matches(&context)
        );
        assert!(
            !Matcher::Header {
                name: "absent".into(),
                value: None
            }
            .matches(&context)
        );
        assert!(
            Matcher::Cookie {
                name: "session".into(),
                value: Some("abc".into())
            }
            .matches(&context)
        );
    }

    #[test]
    fn query_matchers_read_the_query_string() {
        let headers = Headers(HashMap::new());
        let context = context("/", &headers);
        assert!(
            Matcher::Query {
                name: "a".into(),
                value: Some("1".into())
            }
            .matches(&context)
        );
        assert!(
            !Matcher::Query {
                name: "a".into(),
                value: Some("9".into())
            }
            .matches(&context)
        );
        assert!(
            Matcher::Not(Box::new(Matcher::Query {
                name: "z".into(),
                value: None
            }))
            .matches(&context)
        );
    }

    #[test]
    fn cidr_matching_covers_v4_and_v6() {
        let headers = Headers(HashMap::new());
        let context = context("/", &headers);
        let (network, prefix) = parse_cidr("10.0.0.0/8").unwrap();
        assert!(Matcher::ClientIp { network, prefix }.matches(&context));
        let (network, prefix) = parse_cidr("192.168.0.0/16").unwrap();
        assert!(!Matcher::ClientIp { network, prefix }.matches(&context));
        assert!(parse_cidr("10.0.0.1").unwrap() == ("10.0.0.1".parse().unwrap(), 32));
        assert!(parse_cidr("::1/129").is_err());
        assert!(ip_in_network(
            "2001:db8::1".parse().unwrap(),
            "2001:db8::".parse().unwrap(),
            32
        ));
    }

    #[test]
    fn match_nodes_decode_and_sort_deterministically() {
        let origin = Origin::synthetic("test");
        let node = Node::new("match", origin.clone())
            .with_prop("query", gatel_model::Value::string("beta=1", origin.clone()))
            .with_prop("header", gatel_model::Value::string("X-Canary=1", origin.clone()));
        let mut diagnostics = Diagnostics::new();
        let matchers = parse_match_node(&node, &mut diagnostics);
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(matchers[0].describe(), "header x-canary=1");
        assert_eq!(matchers[1].describe(), "query beta=1");
    }

    #[test]
    fn an_unknown_match_condition_suggests_a_known_one() {
        let origin = Origin::synthetic("test");
        let node =
            Node::new("match", origin.clone()).with_prop("headers", gatel_model::Value::string("a", origin));
        let mut diagnostics = Diagnostics::new();
        parse_match_node(&node, &mut diagnostics);
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("did you mean `header`?")
        );
    }
}
