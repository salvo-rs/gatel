//! Where a backend is, and how a name for one is looked up again.
//!
//! The distinction this file exists to keep is between the two: an address is
//! a fact the configuration states, and a name's addresses are a fact the
//! network states and restates. Putting them in one type would make a
//! container being replaced look like a configuration change.

use std::net::SocketAddr;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::Hasher;

/// Where a backend is, as the configuration wrote it.
///
/// A name is not turned into an address here. Resolution is a runtime fact
/// that changes without the configuration changing — a container is replaced
/// and the same name means a different address — so it belongs with health
/// rather than with the plan. What the plan fixes is the *identity*: the name
/// is the endpoint, whatever it currently resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A numeric address, used exactly as written.
    Address(SocketAddr),
    /// A name, resolved through the system resolver so that a container's
    /// search domains, `/etc/hosts` and `nsswitch` mean the same thing here as
    /// they do for every other process on the machine.
    Name { host: String, port: u16 },
    /// A Unix domain socket, written `unix:/run/app.sock`.
    ///
    /// How PHP-FPM, gunicorn, uWSGI and a great many sidecars are reached. It
    /// is not a network address: there is no port, no resolution, and the
    /// permission on the socket file is the access control.
    Unix(std::path::PathBuf),
}

impl Target {
    /// Parse `127.0.0.1:8080`, `[::1]:8080`, `:8080` or `api.internal:8080`.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("`unix:` needs a path, as in `unix:/run/app.sock`".to_string());
            }
            // Refused here rather than at the first connection, and named as a
            // platform limit rather than a mistake in the configuration: this
            // file is right, and this machine cannot run it.
            if !cfg!(unix) {
                return Err(format!(
                    "`{text}` is a Unix domain socket, which this build cannot open — \
                     Unix sockets need a Unix-like operating system"
                ));
            }
            return Ok(Target::Unix(std::path::PathBuf::from(path)));
        }
        if let Ok(address) = gatel_model::parse_socket_addr(text) {
            return Ok(Target::Address(address));
        }
        // Anything with a bracket was meant to be an IPv6 literal, so saying
        // "that is not a name" would answer a question nobody asked.
        if text.starts_with('[') {
            return Err(format!("`{text}` is not a usable `[address]:port`"));
        }
        let Some((host, port)) = text.rsplit_once(':') else {
            return Err(format!("`{text}` needs a port, as in `{text}:8080`"));
        };
        let port: u16 = port
            .parse()
            .map_err(|_| format!("`{port}` is not a port number"))?;
        if port == 0 {
            return Err(format!("`{text}` must use a non-zero port"));
        }
        check_host(host)?;
        Ok(Target::Name {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port,
        })
    }

    /// How it was written, and how it is named everywhere a person reads it.
    pub fn describe(&self) -> String {
        match self {
            Target::Address(address) => address.to_string(),
            Target::Name { host, port } => format!("{host}:{port}"),
            Target::Unix(path) => format!("unix:{}", path.display()),
        }
    }

    /// The port, for the two kinds that have one.
    pub fn port(&self) -> Option<u16> {
        match self {
            Target::Address(address) => Some(address.port()),
            Target::Name { port, .. } => Some(*port),
            Target::Unix(_) => None,
        }
    }

    /// The socket path, for a target that is one.
    pub fn unix_path(&self) -> Option<&std::path::Path> {
        match self {
            Target::Unix(path) => Some(path.as_path()),
            _ => None,
        }
    }

    pub fn is_unix(&self) -> bool {
        matches!(self, Target::Unix(_))
    }

    /// The name, when there is one. This is also the name a certificate would
    /// have to carry, which is why `tls` can take it rather than repeat it.
    pub fn host(&self) -> Option<&str> {
        match self {
            Target::Name { host, .. } => Some(host),
            _ => None,
        }
    }

    pub fn is_name(&self) -> bool {
        matches!(self, Target::Name { .. })
    }

    /// The address, for a target that already is one.
    pub fn address(&self) -> Option<SocketAddr> {
        match self {
            Target::Address(address) => Some(*address),
            _ => None,
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.describe());
    }
}

/// A host name the system resolver could actually be asked about.
///
/// Checked here rather than left to the resolver so a typo is a configuration
/// error with a position, instead of a lookup that fails once a minute.
fn check_host(host: &str) -> Result<(), String> {
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() {
        return Err("a backend needs a host before the port".to_string());
    }
    if trimmed.len() > 253 {
        return Err(format!("`{host}` is longer than a host name may be"));
    }
    for label in trimmed.split('.') {
        if label.is_empty() {
            return Err(format!("`{host}` has an empty label"));
        }
        if label.len() > 63 {
            return Err(format!("`{label}` is longer than a label may be"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("`{label}` may not start or end with `-`"));
        }
        if let Some(bad) = label
            .chars()
            .find(|character| !character.is_ascii_alphanumeric() && *character != '-')
        {
            return Err(format!("`{host}` cannot contain `{bad}`"));
        }
    }
    Ok(())
}

/// Which addresses of a name are usable, when it has several kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Family {
    #[default]
    Any,
    Ipv4,
    Ipv6,
}

impl Family {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "any" => Some(Family::Any),
            "ipv4" => Some(Family::Ipv4),
            "ipv6" => Some(Family::Ipv6),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Family::Any => "any",
            Family::Ipv4 => "ipv4",
            Family::Ipv6 => "ipv6",
        }
    }

    pub fn allows(self, address: &std::net::IpAddr) -> bool {
        match self {
            Family::Any => true,
            Family::Ipv4 => address.is_ipv4(),
            Family::Ipv6 => address.is_ipv6(),
        }
    }
}

/// How a named backend is looked up again.
///
/// On an interval rather than on a TTL: the system resolver is asked through
/// the platform's own path, which is what makes search domains and `/etc/hosts`
/// work, and that path hands back addresses without the TTL that produced
/// them. An interval that is stated is better than a TTL that is invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolve {
    pub interval: Duration,
    pub timeout: Duration,
    pub family: Family,
}

impl Default for Resolve {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            family: Family::Any,
        }
    }
}

impl Resolve {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_duration(self.interval);
        hasher.update_duration(self.timeout);
        hasher.update_str(self.family.as_str());
    }
}

#[derive(Debug, FromNode)]
pub(crate) struct ResolveNode {
    #[gatel(prop)]
    pub interval: Option<Duration>,
    #[gatel(prop)]
    pub timeout: Option<Duration>,
    #[gatel(prop)]
    pub family: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_address_stays_an_address() {
        let target = Target::parse("127.0.0.1:8080").unwrap();
        assert_eq!(target, Target::Address("127.0.0.1:8080".parse().unwrap()));
        assert!(!target.is_name());
        assert_eq!(target.host(), None);
    }

    #[test]
    fn an_ipv6_literal_stays_an_address() {
        let target = Target::parse("[::1]:8080").unwrap();
        assert_eq!(target.port(), Some(8080));
        assert!(!target.is_name());
    }

    #[test]
    fn a_name_is_kept_as_a_name() {
        let target = Target::parse("api.internal:8080").unwrap();
        assert_eq!(target.host(), Some("api.internal"));
        assert_eq!(target.port(), Some(8080));
        assert_eq!(target.describe(), "api.internal:8080");
    }

    #[test]
    fn a_name_is_compared_without_case_or_a_trailing_dot() {
        // The resolver treats these as the same name, so two spellings must
        // not become two endpoints with separate health.
        assert_eq!(
            Target::parse("API.Internal.:8080").unwrap(),
            Target::parse("api.internal:8080").unwrap()
        );
    }

    #[test]
    fn a_single_label_name_is_allowed() {
        // `api:8080` inside a compose network, where the search domain does
        // the rest.
        assert_eq!(Target::parse("api:8080").unwrap().host(), Some("api"));
    }

    #[test]
    fn a_name_without_a_port_says_to_give_one() {
        let error = Target::parse("api.internal").unwrap_err();
        assert!(error.contains("needs a port"), "{error}");
        assert!(error.contains("api.internal:8080"), "{error}");
    }

    #[test]
    fn a_url_is_refused_rather_than_read_as_a_name() {
        let error = Target::parse("http://api.internal:8080").unwrap_err();
        assert!(error.contains("cannot contain"), "{error}");
    }

    #[test]
    fn a_path_is_refused() {
        let error = Target::parse("api.internal:8080/v1").unwrap_err();
        assert!(error.contains("not a port number"), "{error}");
    }

    #[test]
    fn a_broken_ipv6_literal_is_not_reported_as_a_bad_name() {
        let error = Target::parse("[::1:8080").unwrap_err();
        assert!(error.contains("[address]:port"), "{error}");
    }

    #[test]
    fn port_zero_is_refused_for_a_name_as_it_is_for_an_address() {
        assert!(Target::parse("api.internal:0").unwrap_err().contains("non-zero"));
    }

    #[test]
    fn a_label_that_could_never_resolve_is_refused_here_rather_than_hourly() {
        for bad in ["-api.internal:80", "api-.internal:80", "api..internal:80"] {
            assert!(Target::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_family_filters_what_it_says() {
        let v4: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let v6: std::net::IpAddr = "::1".parse().unwrap();
        assert!(Family::Any.allows(&v4) && Family::Any.allows(&v6));
        assert!(Family::Ipv4.allows(&v4) && !Family::Ipv4.allows(&v6));
        assert!(!Family::Ipv6.allows(&v4) && Family::Ipv6.allows(&v6));
    }

    #[test]
    fn two_spellings_of_one_target_hash_alike() {
        let mut left = Hasher::new();
        let mut right = Hasher::new();
        Target::parse("API.internal.:80").unwrap().hash_into(&mut left);
        Target::parse("api.internal:80").unwrap().hash_into(&mut right);
        assert_eq!(left.finish(), right.finish());
    }
}
