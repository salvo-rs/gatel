use std::net::IpAddr;

use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// A parsed CIDR range (e.g. "10.0.0.0/8" or "192.168.1.0/24").
#[derive(Debug, Clone)]
struct CidrRange {
    addr: IpAddr,
    prefix_len: u8,
}

impl CidrRange {
    /// Parse a CIDR string like "192.168.1.0/24" or a bare IP "10.0.0.1".
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some((addr_str, prefix_str)) = s.split_once('/') {
            let addr: IpAddr = addr_str.parse().ok()?;
            let prefix_len: u8 = prefix_str.parse().ok()?;
            // Validate prefix length.
            let max = match addr {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            if prefix_len > max {
                return None;
            }
            Some(CidrRange { addr, prefix_len })
        } else {
            // Bare IP — treat as /32 or /128.
            let addr: IpAddr = s.parse().ok()?;
            let prefix_len = match addr {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            Some(CidrRange { addr, prefix_len })
        }
    }

    /// Check whether a given IP falls within this CIDR range.
    fn contains(&self, ip: &IpAddr) -> bool {
        match (&self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(target)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let net_bits = u32::from(*net);
                let target_bits = u32::from(*target);
                let mask = u32::MAX << (32 - self.prefix_len);
                (net_bits & mask) == (target_bits & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(target)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let net_bits = u128::from(*net);
                let target_bits = u128::from(*target);
                let mask = u128::MAX << (128 - self.prefix_len);
                (net_bits & mask) == (target_bits & mask)
            }
            // v4 vs v6 mismatch: check if v6 is a mapped v4.
            (IpAddr::V4(net), IpAddr::V6(target)) => {
                if let Some(mapped) = target.to_ipv4_mapped() {
                    let net_bits = u32::from(*net);
                    let target_bits = u32::from(mapped);
                    if self.prefix_len == 0 {
                        return true;
                    }
                    let mask = u32::MAX << (32 - self.prefix_len);
                    (net_bits & mask) == (target_bits & mask)
                } else {
                    false
                }
            }
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// IP allow/deny filter middleware.
///
/// If an allow list is configured, only IPs matching the allow list are
/// permitted (deny-by-default). If only a deny list is configured, all IPs
/// are permitted except those in the deny list.
///
/// The deny list is checked first — if an IP is explicitly denied, it is
/// blocked even if it also matches the allow list.
///
/// When `use_forwarded_for` is true the middleware reads the first IP from the
/// `X-Forwarded-For` header instead of using the direct client address,
/// falling back to the client address if the header is absent or unparseable.
pub struct IpFilterHoop {
    allow: Vec<CidrRange>,
    deny: Vec<CidrRange>,
    use_forwarded_for: bool,
}

impl IpFilterHoop {
    pub fn new(allow: &[String], deny: &[String], use_forwarded_for: bool) -> Self {
        let allow: Vec<CidrRange> = allow
            .iter()
            .filter_map(|s| {
                let parsed = CidrRange::parse(s);
                if parsed.is_none() {
                    tracing::warn!(cidr = s.as_str(), "failed to parse allow CIDR, skipping");
                }
                parsed
            })
            .collect();

        let deny: Vec<CidrRange> = deny
            .iter()
            .filter_map(|s| {
                let parsed = CidrRange::parse(s);
                if parsed.is_none() {
                    tracing::warn!(cidr = s.as_str(), "failed to parse deny CIDR, skipping");
                }
                parsed
            })
            .collect();

        debug!(
            allow_count = allow.len(),
            deny_count = deny.len(),
            use_forwarded_for,
            "IP filter configured"
        );

        Self {
            allow,
            deny,
            use_forwarded_for,
        }
    }

    /// Check whether an IP is permitted.
    fn is_allowed(&self, ip: &IpAddr) -> bool {
        // Deny list takes priority.
        for cidr in &self.deny {
            if cidr.contains(ip) {
                return false;
            }
        }

        // If allow list is non-empty, the IP must match at least one entry.
        if !self.allow.is_empty() {
            return self.allow.iter().any(|cidr| cidr.contains(ip));
        }

        // No allow list — permit by default (only deny list applies).
        true
    }
}

#[async_trait]
impl salvo::Handler for IpFilterHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let client = super::client_addr(req);

        // Determine the effective client IP.
        let ip = if self.use_forwarded_for {
            req.headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<IpAddr>().ok())
                .unwrap_or_else(|| client.ip())
        } else {
            client.ip()
        };

        if !self.is_allowed(&ip) {
            debug!(client_ip = %ip, "IP denied by filter, returning 403");
            res.status_code(StatusCode::FORBIDDEN);
            res.body("Forbidden");
            ctrl.skip_rest();
            return;
        }

        ctrl.call_next(req, depot, res).await;
    }
}
