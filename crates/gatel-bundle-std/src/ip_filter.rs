//! `ip-filter`: allow or deny by client address.
//!
//! It reads the canonical client address the dispatcher decided on, not a
//! header. A filter that believed a forwarding header from an untrusted peer
//! would be a filter anyone could walk through.

use std::net::IpAddr;
use std::sync::Arc;

use gatel_model::{Code, Diagnostic, Diagnostics, Hasher, Node};
use gatel_plan::{Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes, parse_cidr};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Network {
    address: IpAddr,
    prefix: u8,
}

impl Network {
    fn contains(&self, ip: IpAddr) -> bool {
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
        match (ip, self.address) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                masked(&a.octets(), self.prefix) == masked(&b.octets(), self.prefix)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                masked(&a.octets(), self.prefix) == masked(&b.octets(), self.prefix)
            }
            // A v4 rule says nothing about a v6 client, and pretending
            // otherwise would either allow or deny by accident.
            _ => false,
        }
    }

    fn describe(&self) -> String {
        format!("{}/{}", self.address, self.prefix)
    }
}

#[derive(Debug)]
pub struct Config {
    allow: Vec<Network>,
    deny: Vec<Network>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_unordered(&self.allow, |network, hasher| {
            hasher.update_str(&network.describe());
        });
        hasher.update_unordered(&self.deny, |network, hasher| {
            hasher.update_str(&network.describe());
        });
    }

    fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(IpFilterHandler {
            allow: self.allow.clone(),
            deny: self.deny.clone(),
        }))
    }

    fn describe(&self) -> String {
        format!("ip-filter allow {} deny {}", self.allow.len(), self.deny.len())
    }
}

pub struct IpFilter;

impl Directive for IpFilter {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("ip-filter", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let errors_before = diagnostics.error_count();
        let mut allow = Vec::new();
        let mut deny = Vec::new();

        for property in &node.props {
            let target = match property.name.as_str() {
                "allow" => &mut allow,
                "deny" => &mut deny,
                other => {
                    let mut diagnostic = Diagnostic::error(
                        Code::UnknownProperty,
                        property.origin.clone(),
                        format!("`ip-filter` has no property `{other}`"),
                    );
                    if let Some(suggestion) = gatel_model::did_you_mean(other, ["allow", "deny"]) {
                        diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                    }
                    diagnostics.push(diagnostic);
                    continue;
                }
            };
            let Some(raw) = property.value.as_str() else {
                diagnostics.error(
                    Code::InvalidValue,
                    property.origin.clone(),
                    "a network is written as a string, such as \"10.0.0.0/8\"",
                );
                continue;
            };
            for entry in raw.split(',') {
                match parse_cidr(entry.trim()) {
                    Ok((address, prefix)) => target.push(Network { address, prefix }),
                    Err(message) => diagnostics.error(Code::InvalidValue, property.origin.clone(), message),
                }
            }
        }

        if allow.is_empty() && deny.is_empty() && diagnostics.error_count() == errors_before {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`ip-filter` needs at least one `allow` or `deny`",
                )
                .with_help("an empty filter would either block everything or nothing, silently"),
            );
        }
        if !node.args.is_empty() {
            diagnostics.error(
                Code::ArgumentCount,
                node.args[0].origin.clone(),
                "`ip-filter` takes `allow` and `deny` properties, not arguments",
            );
        }

        if diagnostics.error_count() > errors_before {
            return None;
        }
        Some(Arc::new(Config { allow, deny }))
    }
}

struct IpFilterHandler {
    allow: Vec<Network>,
    deny: Vec<Network>,
}

impl IpFilterHandler {
    /// Deny wins over allow, and an allow list that exists excludes everything
    /// it does not name. Both are the safe reading of what was written.
    fn permits(&self, ip: IpAddr) -> bool {
        if self.deny.iter().any(|network| network.contains(ip)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|network| network.contains(ip))
    }
}

#[async_trait]
impl Handler for IpFilterHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        // The dispatcher decided which address to believe. A request with no
        // address at all cannot be checked, so it is refused.
        let permitted = depot
            .get::<IpAddr>("gatel.client_ip")
            .ok()
            .copied()
            .is_some_and(|ip| self.permits(ip));
        if permitted {
            ctrl.call_next(request, depot, response).await;
            return;
        }
        // 403 without saying which rule matched: the filter's shape is not the
        // client's business.
        crate::generated(depot);
        response.status_code(StatusCode::FORBIDDEN);
        let _ = response.write_body("403 forbidden\n");
        ctrl.skip_rest();
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    /// A context with nothing in it, for a directive that needs nothing.
    fn empty_context() -> gatel_plan::BuildContext<'static> {
        gatel_plan::BuildContext {
            telemetry: crate::testing::telemetry(),
            upstreams: crate::testing::no_upstreams(),
            root: std::path::Path::new("."),
            state: None,
        }
    }
    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = IpFilter.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn filter(allow: Option<&str>, deny: Option<&str>) -> IpFilterHandler {
        let mut node = Node::new("ip-filter", origin());
        if let Some(allow) = allow {
            node = node.with_prop("allow", Value::string(allow, origin()));
        }
        if let Some(deny) = deny {
            node = node.with_prop("deny", Value::string(deny, origin()));
        }
        let config = decode(node).0.expect("valid filter");
        let any = config.build(empty_context()).unwrap();
        let _ = any;
        // Rebuild the handler directly so the decision function can be tested.
        let mut allow_networks = Vec::new();
        let mut deny_networks = Vec::new();
        for (raw, target) in [(allow, &mut allow_networks), (deny, &mut deny_networks)] {
            if let Some(raw) = raw {
                for entry in raw.split(',') {
                    let (address, prefix) = parse_cidr(entry.trim()).unwrap();
                    target.push(Network { address, prefix });
                }
            }
        }
        IpFilterHandler {
            allow: allow_networks,
            deny: deny_networks,
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn an_allow_list_excludes_everything_it_does_not_name() {
        let filter = filter(Some("10.0.0.0/8"), None);
        assert!(filter.permits(ip("10.1.2.3")));
        assert!(!filter.permits(ip("192.168.1.1")));
    }

    #[test]
    fn a_deny_list_blocks_only_what_it_names() {
        let filter = filter(None, Some("192.168.0.0/16"));
        assert!(!filter.permits(ip("192.168.1.1")));
        assert!(filter.permits(ip("10.1.2.3")));
    }

    #[test]
    fn deny_wins_over_allow() {
        let filter = filter(Some("10.0.0.0/8"), Some("10.9.0.0/16"));
        assert!(filter.permits(ip("10.1.2.3")));
        assert!(!filter.permits(ip("10.9.0.5")), "the narrower deny must win");
    }

    #[test]
    fn several_networks_may_be_listed_at_once() {
        let filter = filter(Some("10.0.0.0/8, 172.16.0.0/12"), None);
        assert!(filter.permits(ip("10.0.0.1")));
        assert!(filter.permits(ip("172.16.5.5")));
        assert!(!filter.permits(ip("192.168.0.1")));
    }

    #[test]
    fn a_rule_for_one_family_says_nothing_about_the_other() {
        let filter = filter(Some("0.0.0.0/0"), None);
        assert!(filter.permits(ip("1.2.3.4")));
        assert!(
            !filter.permits(ip("2001:db8::1")),
            "a v4 rule must not silently admit v6"
        );
    }

    #[test]
    fn a_single_address_is_a_full_length_prefix() {
        let filter = filter(Some("203.0.113.7"), None);
        assert!(filter.permits(ip("203.0.113.7")));
        assert!(!filter.permits(ip("203.0.113.8")));
    }

    #[test]
    fn an_empty_filter_is_refused_rather_than_guessed_at() {
        let (config, diagnostics) = decode(Node::new("ip-filter", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("silently")
        );
    }

    #[test]
    fn a_malformed_network_is_reported_with_the_offending_text() {
        let node =
            Node::new("ip-filter", origin()).with_prop("allow", Value::string("10.0.0.0/99", origin()));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("/99")));
    }

    #[test]
    fn an_unknown_property_suggests_a_known_one() {
        let node = Node::new("ip-filter", origin()).with_prop("alow", Value::string("10.0.0.0/8", origin()));
        let (_, diagnostics) = decode(node);
        assert_eq!(
            diagnostics.iter().next().unwrap().help.as_deref(),
            Some("did you mean `allow`?")
        );
    }

    #[test]
    fn the_order_networks_are_written_in_does_not_change_the_fingerprint() {
        let hash = |allow: &str| {
            let node = Node::new("ip-filter", origin()).with_prop("allow", Value::string(allow, origin()));
            let mut hasher = Hasher::new();
            decode(node).0.unwrap().hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(
            hash("10.0.0.0/8, 172.16.0.0/12"),
            hash("172.16.0.0/12, 10.0.0.0/8")
        );
    }
}
