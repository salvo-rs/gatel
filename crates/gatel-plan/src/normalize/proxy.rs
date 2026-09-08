//! Strict proxy forms and inline upstream construction.
use gatel_model::{Code, Diagnostic, Node, ResourceId, ResourceKind, SourceId, Value};

use super::Context;
use crate::spec::Terminal;
use crate::upstream::{Upstream, anonymous_upstream_id, decode_upstream_body};

impl Context<'_> {
    /// Address, named upstream and inline body all lower to an Upstream resource.
    pub(super) fn proxy(
        &mut self,
        node: &Node,
        site: &ResourceId,
        owner: &SourceId,
        upstreams: &mut Vec<Upstream>,
    ) -> Option<Terminal> {
        let named = node.prop("upstream");
        let inline_address = node.args.first();

        let mut seen_upstream = false;
        for property in &node.props {
            if property.name != "upstream" {
                self.error(Code::UnknownProperty, &property.origin, "`proxy` only accepts the upstream property; configure inline upstream options with child nodes");
                return None;
            }
            if seen_upstream {
                self.error(
                    Code::Duplicate,
                    &property.origin,
                    "`proxy` upstream is declared twice",
                );
                return None;
            }
            seen_upstream = true;
        }

        if node.args.len() > 1 {
            self.error(
                Code::ArgumentCount,
                &node.origin,
                "`proxy` takes at most one inline address; use endpoint nodes for multiple backends",
            );
            return None;
        }

        if named.is_some() && inline_address.is_some() {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    "`proxy` takes either an address or `upstream=`, not both",
                )
                .with_help(
                    "an inline address defines a private upstream; `upstream=` refers to a shared one",
                ),
            );
            return None;
        }

        if let Some(property) = named {
            let Some(name) = property.value.as_str() else {
                self.error(Code::InvalidValue, &property.origin, "`upstream` must be a name");
                return None;
            };
            if !node.children.is_empty() {
                self.error(
                    Code::UnknownNode,
                    &node.children[0].origin,
                    "`proxy upstream=` refers to an upstream declared elsewhere; it takes no body",
                );
                return None;
            }
            return Some(Terminal::Proxy {
                upstream: ResourceId::new(ResourceKind::Upstream, name),
            });
        }

        // An inline address is shorthand for a one-endpoint body, so the two
        // forms share their entire decoder.
        let body = match inline_address {
            Some(value) => {
                let Some(address) = value.as_str() else {
                    self.error(
                        Code::InvalidValue,
                        &value.origin,
                        "`proxy` takes an address string",
                    );
                    return None;
                };
                if !node.children.is_empty() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::InvalidValue,
                            node.children[0].origin.clone(),
                            "`proxy` takes either an inline address or a body",
                        )
                        .with_help("move the address into an `endpoint` node inside the body"),
                    );
                    return None;
                }
                let mut synthetic = Node::new("proxy", node.origin.clone());
                synthetic.children.push(
                    Node::new("endpoint", value.origin.clone())
                        .with_arg(Value::string(address, value.origin.clone())),
                );
                synthetic
            }
            None => {
                if node.children.is_empty() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            Code::ArgumentCount,
                            node.origin.clone(),
                            "`proxy` needs an address, `upstream=`, or a body",
                        )
                        .with_help("`proxy \"127.0.0.1:9000\"` is the short form"),
                    );
                    return None;
                }
                node.clone()
            }
        };

        let addresses: Vec<String> = body
            .children_named("endpoint")
            .filter_map(|child| child.args.first().and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();
        let seed = anonymous_upstream_id(site, owner, &addresses);
        let mut upstream = decode_upstream_body(&body, seed, owner.clone(), self.diagnostics)?;
        // Address equality alone does not imply equal transport policy. Derive
        // the final identity from the complete decoded configuration, including
        // defaults, before deciding whether a runtime/pool may be shared.
        let mut identity = gatel_model::Hasher::new();
        identity.update_str("inline-upstream-policy-v1");
        upstream.hash_into(&mut identity);
        let id = ResourceId::derived_owned(
            ResourceKind::Upstream,
            owner,
            &[site.name(), &identity.finish().to_string()],
        );
        upstream.id = id.clone();

        // Share only equivalent upstreams, never first-declaration-wins policy.
        if !upstreams.iter().any(|existing| existing.id == id) {
            upstreams.push(upstream);
        }
        Some(Terminal::Proxy { upstream: id })
    }
}
