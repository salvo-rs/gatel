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
use crate::spec::{Acme, Global, Limits, Listener, ListenerProtocol, Log};
use crate::upstream::{Upstream, decode_upstream_body};

#[derive(Debug, FromNode)]
struct AcmeNode {
    /// The CA's directory URL. Defaults to Let's Encrypt.
    #[gatel(prop)]
    ca: Option<String>,
    #[gatel(prop)]
    email: Option<String>,
    /// The CA's terms of service. There is no default: agreeing on an
    /// operator's behalf is not this program's to do.
    #[gatel(prop, default = false)]
    agreed: bool,
    /// Where the account key and the certificates are kept.
    #[gatel(prop)]
    directory: Option<String>,
    #[gatel(prop)]
    renew_before: Option<Duration>,
}

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
                "admin" => self.admin(child),
                "acme" => {
                    if let Some(acme) = self.acme(child) {
                        global.acme = Some(acme);
                    }
                }
                other => {
                    const KNOWN: [&str; 8] = [
                        "http",
                        "https",
                        "grace-period",
                        "trusted-proxy",
                        "limits",
                        "log",
                        "admin",
                        "acme",
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
        let mut proxy_protocol = false;
        for property in &node.props {
            match property.name.as_str() {
                "proxy-protocol" => match property.value.as_bool() {
                    Some(enabled) => proxy_protocol = enabled,
                    None => self.error(
                        Code::InvalidValue,
                        &property.origin,
                        "`proxy-protocol` is `#true` or `#false`",
                    ),
                },
                other => self.error(
                    Code::UnknownProperty,
                    &property.origin,
                    format!("`{}` has no property `{other}`", node.name),
                ),
            }
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
            proxy_protocol,
            origin: node.origin.clone(),
        })
    }

    /// Check an `admin` block, and then refuse it.
    ///
    /// Nothing in this binary serves an admin API. Accepting the block anyway
    /// would leave an operator believing they had opened a control socket that
    /// does not exist, which is the one outcome worse than saying no — so the
    /// block is still validated, so its address and token are diagnosed if they
    /// are also wrong, and then refused for what it actually is.
    pub(super) fn admin(&mut self, node: &Node) {
        self.check_admin(node);
        self.diagnostics.push(
            Diagnostic::error(
                Code::UnknownNode,
                node.origin.clone(),
                "this build has no admin API",
            )
            .with_help(
                "reload by editing the configuration file or sending SIGHUP; \
                 expose status with the `metrics` and `health` directives",
            ),
        );
    }

    fn check_admin(&mut self, node: &Node) -> Option<()> {
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
        match text.strip_prefix("unix:") {
            Some(_) => {}
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
                }
                Err(message) => {
                    self.error(Code::InvalidValue, &node.args[0].origin, message);
                    return None;
                }
            },
        }
        if let Some(property) = node.prop("token")
            && let Some(text) = property.value.as_str()
            && let Err(message) = SecretRef::parse(text)
        {
            self.diagnostics
                .error(Code::InvalidSecret, property.origin.clone(), message);
        }
        Some(())
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

impl Context<'_> {
    /// `acme email="..." agreed=#true`, once, for the whole process.
    pub(super) fn acme(&mut self, node: &Node) -> Option<Acme> {
        let fields = AcmeNode::from_node(node, self.diagnostics)?;

        let Some(email) = fields.email.filter(|email| email.contains('@')) else {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`acme` needs an `email`",
                )
                .with_help(
                    "the CA writes to it about expiry and problems; a certificate nobody can be                      told about is one that expires quietly",
                ),
            );
            return None;
        };
        if !fields.agreed {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`acme` needs `agreed=#true`",
                )
                .with_help(
                    "issuing a certificate accepts the CA's terms of service, and this program                      will not accept them on your behalf",
                ),
            );
            return None;
        }

        let directory = fields.ca.unwrap_or_else(|| Acme::DEFAULT_DIRECTORY.to_string());
        if !directory.starts_with("https://") && !directory.starts_with("http://") {
            self.error(
                Code::InvalidValue,
                &node.origin,
                format!("`{directory}` is not a directory URL"),
            );
            return None;
        }

        // Two thirds through the life of a 90-day certificate, which leaves a
        // month to notice that renewal is not working.
        let renew_before = fields.renew_before.unwrap_or(Duration::from_secs(30 * 86_400));
        if renew_before.is_zero() {
            self.error(
                Code::InvalidValue,
                &node.origin,
                "`renew-before` of zero would renew only after the certificate had expired",
            );
            return None;
        }

        Some(Acme {
            directory,
            email,
            agreed: true,
            directory_path: fields.directory,
            renew_before,
        })
    }
}
