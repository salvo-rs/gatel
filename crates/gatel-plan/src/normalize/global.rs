//! The `global` block: listeners, limits, logging and the admin entry point.
//!
//! Separated from route decoding because these are process-wide settings
//! rather than resources, and because they are the part a reload most often
//! must *not* change: a listener's address decides whether a socket can be
//! reused at all.

use std::net::SocketAddr;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{
    ByteSize, Code, Diagnostic, FromNode as _, Node, ResourceId, ResourceKind, SecretRef, SourceId,
    did_you_mean,
};

use super::Context;
use crate::matcher::parse_cidr;
use crate::spec::{Admin, AdminBind, Global, Limits, Listener, ListenerProtocol, Log};
use crate::upstream::{Upstream, decode_upstream_body};

#[derive(Debug, FromNode)]
struct LimitsNode {
    #[gatel(prop)]
    header: Option<ByteSize>,
    #[gatel(prop)]
    body: Option<ByteSize>,
    #[gatel(prop)]
    handshake_timeout: Option<Duration>,
    #[gatel(prop)]
    idle: Option<Duration>,
    #[gatel(prop)]
    connections: Option<u32>,
}

#[derive(Debug, FromNode)]
struct LogNode {
    #[gatel(prop, default = String::from("info"))]
    level: String,
    #[gatel(prop, default = String::from("text"))]
    format: String,
}

impl Context<'_> {
    pub(super) fn global(&mut self, node: &Node, owner: &SourceId) -> Option<(Global, Vec<Listener>)> {
        let errors_before = self.diagnostics.error_count();
        let mut global = Global::default();
        let mut listeners: Vec<Listener> = Vec::new();

        if !node.args.is_empty() {
            self.error(
                Code::ArgumentCount,
                &node.args[0].origin,
                "`global` takes no arguments",
            );
        }
        for property in &node.props {
            self.error(
                Code::UnknownProperty,
                &property.origin,
                format!("`global` has no property `{}`; use child nodes", property.name),
            );
        }

        for child in &node.children {
            match child.name.as_str() {
                "http" | "https" => {
                    let protocol = if child.name == "http" {
                        ListenerProtocol::Http
                    } else {
                        ListenerProtocol::Https
                    };
                    if let Some(listener) = self.listener(child, owner, protocol) {
                        listeners.push(listener);
                    }
                }
                "grace-period" => {
                    if let Some(duration) = self.single_duration(child) {
                        global.grace_period = duration;
                    }
                }
                "trusted-proxy" => {
                    for value in &child.args {
                        let Some(text) = value.as_str() else {
                            self.error(Code::InvalidValue, &value.origin, "expected a CIDR string");
                            continue;
                        };
                        match parse_cidr(text) {
                            Ok(network) => global.trusted_proxies.push(network),
                            Err(message) => self.error(Code::InvalidValue, &value.origin, message),
                        }
                    }
                }
                "limits" => {
                    if let Some(decoded) = LimitsNode::from_node(child, self.diagnostics) {
                        let base = Limits::default();
                        global.limits = Limits {
                            max_header: decoded.header.unwrap_or(base.max_header),
                            max_body: decoded.body.unwrap_or(base.max_body),
                            handshake_timeout: decoded.handshake_timeout.unwrap_or(base.handshake_timeout),
                            idle_timeout: decoded.idle.unwrap_or(base.idle_timeout),
                            max_connections: decoded.connections.unwrap_or(base.max_connections),
                        };
                    }
                }
                "log" => {
                    if let Some(decoded) = LogNode::from_node(child, self.diagnostics) {
                        if !["text", "json", "clf"].contains(&decoded.format.as_str()) {
                            self.error(
                                Code::InvalidValue,
                                &child.origin,
                                format!("unknown log format `{}`; use text, json or clf", decoded.format),
                            );
                        }
                        global.log = Log {
                            level: decoded.level,
                            format: decoded.format,
                        };
                    }
                }
                "admin" => {
                    if let Some(admin) = self.admin(child) {
                        global.admin = Some(admin);
                    }
                }
                other => {
                    const KNOWN: [&str; 7] = [
                        "http",
                        "https",
                        "grace-period",
                        "trusted-proxy",
                        "limits",
                        "log",
                        "admin",
                    ];
                    let mut diagnostic = Diagnostic::error(
                        Code::UnknownNode,
                        child.origin.clone(),
                        format!("`global` has no child node `{other}`"),
                    );
                    if let Some(suggestion) = did_you_mean(other, KNOWN) {
                        diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                    }
                    self.diagnostics.push(diagnostic);
                }
            }
        }

        if listeners.is_empty() {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`global` needs at least one `http` or `https` listener",
                )
                .with_help("write `http \":8080\"`"),
            );
        }

        if self.diagnostics.error_count() > errors_before {
            return None;
        }
        Some((global, listeners))
    }

    /// `http ":80"` names the listener after its protocol;
    /// `http "internal" "127.0.0.1:8081"` names it explicitly.
    pub(super) fn listener(
        &mut self,
        node: &Node,
        owner: &SourceId,
        protocol: ListenerProtocol,
    ) -> Option<Listener> {
        for property in &node.props {
            self.error(
                Code::UnknownProperty,
                &property.origin,
                format!("`{}` has no property `{}`", node.name, property.name),
            );
        }
        let (name, address_value) = match node.args.len() {
            1 => (protocol.as_str().to_string(), &node.args[0]),
            2 => {
                let Some(name) = node.args[0].as_str() else {
                    self.error(
                        Code::InvalidValue,
                        &node.args[0].origin,
                        "the listener name must be a string",
                    );
                    return None;
                };
                (name.to_string(), &node.args[1])
            }
            _ => {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::ArgumentCount,
                        node.origin.clone(),
                        format!("`{}` takes an address, optionally preceded by a name", node.name),
                    )
                    .with_help("`http \":80\"` or `http \"internal\" \"127.0.0.1:8081\"`"),
                );
                return None;
            }
        };
        let Some(text) = address_value.as_str() else {
            self.error(
                Code::InvalidValue,
                &address_value.origin,
                "the listener address must be a string",
            );
            return None;
        };
        let bind: SocketAddr = match gatel_model::parse_socket_addr(text) {
            Ok(bind) => bind,
            Err(message) => {
                self.error(Code::InvalidValue, &address_value.origin, message);
                return None;
            }
        };
        if !node.children.is_empty() {
            self.error(
                Code::UnknownNode,
                &node.children[0].origin,
                format!(
                    "`{}` takes no child nodes; certificates belong to a site",
                    node.name
                ),
            );
        }
        Some(Listener {
            id: ResourceId::new(ResourceKind::Listener, name.clone()),
            owner: owner.clone(),
            name,
            bind,
            protocol,
            origin: node.origin.clone(),
        })
    }

    pub(super) fn admin(&mut self, node: &Node) -> Option<Admin> {
        let Some(text) = node.args.first().and_then(|value| value.as_str()) else {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::ArgumentCount,
                    node.origin.clone(),
                    "`admin` takes a unix socket path or a loopback address",
                )
                .with_help("`admin \"unix:/run/gatel/admin.sock\"`"),
            );
            return None;
        };
        let bind = match text.strip_prefix("unix:") {
            Some(path) => AdminBind::Unix(path.to_string()),
            None => match gatel_model::parse_socket_addr(text) {
                Ok(address) => {
                    if !address.ip().is_loopback() {
                        self.diagnostics.push(
                            Diagnostic::error(
                                Code::InvalidValue,
                                node.args[0].origin.clone(),
                                format!("the admin API may not listen on {}", address.ip()),
                            )
                            .with_help(
                                "bind it to a unix socket or a loopback address and put a proxy in front",
                            ),
                        );
                        return None;
                    }
                    AdminBind::Tcp(address)
                }
                Err(message) => {
                    self.error(Code::InvalidValue, &node.args[0].origin, message);
                    return None;
                }
            },
        };
        let token = node.prop("token").and_then(|property| {
            let text = property.value.as_str()?;
            match SecretRef::parse(text) {
                Ok(reference) => Some(reference),
                Err(message) => {
                    self.diagnostics
                        .error(Code::InvalidSecret, property.origin.clone(), message);
                    None
                }
            }
        });
        Some(Admin { bind, token })
    }

    pub(super) fn single_duration(&mut self, node: &Node) -> Option<Duration> {
        let value = node.args.first()?;
        let text = value.as_str()?;
        match gatel_model::parse_duration(text) {
            Ok(duration) => Some(duration),
            Err(message) => {
                self.error(Code::InvalidValue, &value.origin, message);
                None
            }
        }
    }

    pub(super) fn upstream(&mut self, node: &Node, owner: &SourceId) -> Option<Upstream> {
        let Some(name) = node.args.first().and_then(|value| value.as_str()) else {
            self.error(Code::ArgumentCount, &node.origin, "`upstream` needs a name");
            return None;
        };
        if !owner.owns(name) {
            self.report_ownership(owner, name, &node.args[0].origin);
            return None;
        }
        for property in &node.props {
            self.error(
                Code::UnknownProperty,
                &property.origin,
                format!("`upstream` has no property `{}`", property.name),
            );
        }
        let id = ResourceId::new(ResourceKind::Upstream, name);
        decode_upstream_body(node, id, owner.clone(), self.diagnostics)
    }
}
