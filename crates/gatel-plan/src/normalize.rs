//! Snapshot to spec.
//!
//! This is where a configuration stops being text and becomes a resource graph:
//! names are resolved, identities are assigned, directives are looked up in the
//! registry and sorted into their phases, and every remaining ambiguity is
//! reported rather than guessed at.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{
    ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Node, Origin, ResourceId, ResourceKind,
    SecretRef, Snapshot, SourceId, Value, did_you_mean,
};

use crate::directive::{Registry, Scope};
use crate::matcher::{HostMatch, MethodMatch, PathMatch, compare_specificity, parse_cidr, parse_match_node};
use crate::spec::{
    Admin, AdminBind, CertificateSource, ClientAuth, Global, Limits, Listener, ListenerProtocol, Log, Route,
    Site, SiteTls, Spec, Terminal,
};
use crate::upstream::{Upstream, anonymous_upstream_id, decode_upstream_body};

const TOP_LEVEL: [&str; 3] = ["global", "site", "upstream"];

/// Turn a snapshot into a spec, or into the reasons it cannot become one.
pub fn normalize(snapshot: &Snapshot, registry: &Registry) -> Result<Spec, Diagnostics> {
    let mut diagnostics = Diagnostics::new();
    let mut context = Context {
        registry,
        diagnostics: &mut diagnostics,
    };

    let mut global: Option<(Global, Vec<Listener>, Origin)> = None;
    let mut sites: Vec<Site> = Vec::new();
    let mut upstreams: Vec<Upstream> = Vec::new();

    for (owner, node) in snapshot.nodes() {
        match node.name.as_str() {
            "global" => match &global {
                Some((_, _, first)) => context.diagnostics.push(
                    Diagnostic::error(
                        Code::Duplicate,
                        node.origin.clone(),
                        "`global` may only be declared once",
                    )
                    .with_related("first declared here", first.clone()),
                ),
                None => {
                    if let Some(parsed) = context.global(node, owner) {
                        global = Some((parsed.0, parsed.1, node.origin.clone()));
                    }
                }
            },
            "site" => {
                if let Some(site) = context.site(node, owner, &mut upstreams) {
                    sites.push(site);
                }
            }
            "upstream" => {
                if let Some(upstream) = context.upstream(node, owner) {
                    upstreams.push(upstream);
                }
            }
            other => {
                let mut diagnostic = Diagnostic::error(
                    Code::UnknownNode,
                    node.origin.clone(),
                    format!("`{other}` is not a top-level node"),
                );
                if let Some(suggestion) = did_you_mean(other, TOP_LEVEL) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                } else {
                    diagnostic =
                        diagnostic.with_help(format!("top-level nodes are: {}", TOP_LEVEL.join(", ")));
                }
                context.diagnostics.push(diagnostic);
            }
        }
    }

    let (global, listeners) = match global {
        Some((global, listeners, _)) => (global, listeners),
        None => {
            if !diagnostics.has_errors() {
                diagnostics.push(
                    Diagnostic::error(
                        Code::MissingField,
                        Origin::synthetic("configuration"),
                        "a configuration needs a `global` block with at least one listener",
                    )
                    .with_help("write `global { http \":8080\" }`"),
                );
            }
            return Err(diagnostics.sorted());
        }
    };

    let mut spec = Spec {
        global,
        listeners,
        sites,
        upstreams,
    };
    check_uniqueness(&spec, &mut diagnostics);
    check_references(&spec, &mut diagnostics);
    sort_routes(&mut spec);

    if diagnostics.has_errors() {
        Err(diagnostics.sorted())
    } else {
        Ok(spec)
    }
}

struct Context<'a> {
    registry: &'a Registry,
    diagnostics: &'a mut Diagnostics,
}

#[derive(Debug, FromNode)]
struct LimitsNode {
    #[gatel(prop)]
    header: Option<ByteSize>,
    #[gatel(prop)]
    body: Option<ByteSize>,
    #[gatel(prop)]
    header_timeout: Option<Duration>,
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
    fn error(&mut self, code: Code, origin: &Origin, message: impl Into<String>) {
        self.diagnostics.error(code, origin.clone(), message);
    }

    fn global(&mut self, node: &Node, owner: &SourceId) -> Option<(Global, Vec<Listener>)> {
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
                            header_timeout: decoded.header_timeout.unwrap_or(base.header_timeout),
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
    fn listener(&mut self, node: &Node, owner: &SourceId, protocol: ListenerProtocol) -> Option<Listener> {
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

    fn admin(&mut self, node: &Node) -> Option<Admin> {
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

    fn single_duration(&mut self, node: &Node) -> Option<Duration> {
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

    fn upstream(&mut self, node: &Node, owner: &SourceId) -> Option<Upstream> {
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

    fn report_ownership(&mut self, owner: &SourceId, name: &str, origin: &Origin) {
        let help = match owner.namespace() {
            Some(prefix) => format!("`{owner}` may only name resources under `{prefix}`"),
            None => "the file configuration owns the root namespace; `/` is reserved for other sources"
                .to_string(),
        };
        self.diagnostics.push(
            Diagnostic::error(
                Code::OwnershipConflict,
                origin.clone(),
                format!("`{owner}` may not declare `{name}`"),
            )
            .with_help(help),
        );
    }

    fn site(&mut self, node: &Node, owner: &SourceId, upstreams: &mut Vec<Upstream>) -> Option<Site> {
        let errors_before = self.diagnostics.error_count();

        let Some(raw_host) = node.args.first().and_then(|value| value.as_str()) else {
            self.diagnostics.push(
                Diagnostic::error(Code::ArgumentCount, node.origin.clone(), "`site` needs a host")
                    .with_help("`site \"example.com\"` or `site \"*\"` for any host"),
            );
            return None;
        };
        let host = match HostMatch::parse(raw_host) {
            Ok(host) => host,
            Err(message) => {
                self.error(Code::InvalidValue, &node.args[0].origin, message);
                return None;
            }
        };
        if node.args.len() > 1 {
            self.error(
                Code::ArgumentCount,
                &node.args[1].origin,
                "`site` takes exactly one host",
            );
        }

        let mut listeners = Vec::new();
        let mut explicit_id = None;
        for property in &node.props {
            match property.name.as_str() {
                "listener" => match property.value.as_str() {
                    Some(name) => listeners.push(name.to_string()),
                    None => self.error(Code::InvalidValue, &property.origin, "`listener` must be a name"),
                },
                "id" => match property.value.as_str() {
                    Some(id) => explicit_id = Some(id.to_string()),
                    None => self.error(Code::InvalidValue, &property.origin, "`id` must be a string"),
                },
                other => self.error(
                    Code::UnknownProperty,
                    &property.origin,
                    format!("`site` has no property `{other}`"),
                ),
            }
        }

        let id = match explicit_id {
            Some(name) => {
                if !owner.owns(&name) {
                    self.report_ownership(owner, &name, &node.origin);
                    return None;
                }
                ResourceId::new(ResourceKind::Site, name)
            }
            None => {
                let mut parts = vec![host.describe()];
                parts.extend(listeners.iter().cloned());
                let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
                ResourceId::derived(ResourceKind::Site, &refs)
            }
        };

        let mut tls = None;
        let mut directives = Vec::new();
        let mut routes = Vec::new();

        for child in &node.children {
            match child.name.as_str() {
                "route" => {
                    if let Some(route) = self.route(child, &id, &host, owner, upstreams) {
                        routes.push(route);
                    }
                }
                "tls" => match &tls {
                    Some(_) => self.error(
                        Code::Duplicate,
                        &child.origin,
                        "a site may only declare `tls` once",
                    ),
                    None => tls = self.site_tls(child),
                },
                _ => {
                    if let Some(instance) = self.registry.decode(child, Scope::Site, self.diagnostics) {
                        if instance.meta.role.is_terminal() {
                            self.diagnostics.push(
                                Diagnostic::error(
                                    Code::InvalidScope,
                                    child.origin.clone(),
                                    format!("`{}` is a terminal and belongs in a route", instance.meta.name),
                                )
                                .with_help("a site cannot answer a request by itself"),
                            );
                        } else {
                            directives.push(instance);
                        }
                    }
                }
            }
        }

        if routes.is_empty() {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    format!("site `{}` has no routes", host.describe()),
                )
                .with_help("a site with no routes can never answer a request"),
            );
        }

        reject_duplicate_directives(&directives, self.diagnostics);
        directives.sort_by_key(crate::directive::DirectiveInstance::sort_key);

        if self.diagnostics.error_count() > errors_before {
            return None;
        }
        Some(Site {
            id,
            owner: owner.clone(),
            host,
            listeners,
            tls,
            directives,
            routes,
            origin: node.origin.clone(),
        })
    }

    fn site_tls(&mut self, node: &Node) -> Option<SiteTls> {
        let secret = |context: &mut Self, name: &str| -> Option<SecretRef> {
            let property = node.prop(name)?;
            let text = property.value.as_str()?;
            match SecretRef::parse(text) {
                Ok(reference) => Some(reference),
                Err(message) => {
                    context
                        .diagnostics
                        .error(Code::InvalidSecret, property.origin.clone(), message);
                    None
                }
            }
        };

        let keyword = node.args.first().and_then(|value| value.as_str());
        let source = match keyword {
            Some("internal") => CertificateSource::Internal,
            Some("acme") => CertificateSource::Acme,
            Some(other) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.args[0].origin.clone(),
                        format!("unknown certificate source `{other}`"),
                    )
                    .with_help("write `tls internal`, `tls acme`, or `tls cert=\"..\" key=\"..\"`"),
                );
                return None;
            }
            None => {
                let cert = secret(self, "cert");
                let key = secret(self, "key");
                match (cert, key) {
                    (Some(cert), Some(key)) => CertificateSource::Manual { cert, key },
                    _ => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                Code::MissingField,
                                node.origin.clone(),
                                "`tls` needs both `cert` and `key`, or one of `internal` / `acme`",
                            )
                            .with_help("certificate material is referenced, never inlined"),
                        );
                        return None;
                    }
                }
            }
        };

        let client_auth = match node.child("client-auth") {
            Some(child) => {
                let Some(ca) = child.prop("ca").and_then(|property| property.value.as_str()) else {
                    self.error(Code::MissingField, &child.origin, "`client-auth` requires a `ca`");
                    return None;
                };
                let ca = match SecretRef::parse(ca) {
                    Ok(reference) => reference,
                    Err(message) => {
                        self.error(Code::InvalidSecret, &child.origin, message);
                        return None;
                    }
                };
                let required = child
                    .args
                    .first()
                    .and_then(|value| value.as_str())
                    .map(|mode| mode == "required")
                    .unwrap_or(true);
                Some(ClientAuth { required, ca })
            }
            None => None,
        };

        for property in &node.props {
            if !["cert", "key"].contains(&property.name.as_str()) {
                self.error(
                    Code::UnknownProperty,
                    &property.origin,
                    format!("`tls` has no property `{}`", property.name),
                );
            }
        }
        Some(SiteTls { source, client_auth })
    }

    fn route(
        &mut self,
        node: &Node,
        site: &ResourceId,
        host: &HostMatch,
        owner: &SourceId,
        upstreams: &mut Vec<Upstream>,
    ) -> Option<Route> {
        let errors_before = self.diagnostics.error_count();

        let Some(raw_path) = node.args.first().and_then(|value| value.as_str()) else {
            self.diagnostics.push(
                Diagnostic::error(Code::ArgumentCount, node.origin.clone(), "`route` needs a path")
                    .with_help("`route \"/api/*\"` or `route \"/\"`"),
            );
            return None;
        };
        let path = match PathMatch::parse(raw_path) {
            Ok(path) => path,
            Err(message) => {
                self.error(Code::InvalidValue, &node.args[0].origin, message);
                return None;
            }
        };

        let mut explicit_id = None;
        for property in &node.props {
            match property.name.as_str() {
                "id" => match property.value.as_str() {
                    Some(id) => explicit_id = Some(id.to_string()),
                    None => self.error(Code::InvalidValue, &property.origin, "`id` must be a string"),
                },
                other => self.error(
                    Code::UnknownProperty,
                    &property.origin,
                    format!("`route` has no property `{other}`"),
                ),
            }
        }

        let mut methods = MethodMatch::Any;
        let mut method_origin = None;
        let mut matchers = Vec::new();
        let mut directives = Vec::new();
        let mut terminals: Vec<(Terminal, Origin)> = Vec::new();

        for child in &node.children {
            match child.name.as_str() {
                "method" => {
                    if let Some(first) = &method_origin {
                        self.diagnostics.push(
                            Diagnostic::error(
                                Code::Duplicate,
                                child.origin.clone(),
                                "`method` may only be declared once",
                            )
                            .with_related("first declared here", Origin::clone(first)),
                        );
                        continue;
                    }
                    method_origin = Some(child.origin.clone());
                    let names: Vec<String> = child
                        .args
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect();
                    if names.len() != child.args.len() {
                        self.error(Code::InvalidValue, &child.origin, "methods must be strings");
                        continue;
                    }
                    match MethodMatch::parse(&names) {
                        Ok(parsed) => methods = parsed,
                        Err(message) => self.error(Code::InvalidValue, &child.origin, message),
                    }
                }
                "match" => matchers.extend(parse_match_node(child, self.diagnostics)),
                "proxy" => {
                    if let Some(terminal) = self.proxy(child, site, owner, upstreams) {
                        terminals.push((terminal, child.origin.clone()));
                    }
                }
                _ => {
                    if let Some(instance) = self.registry.decode(child, Scope::Route, self.diagnostics) {
                        if instance.meta.role.is_terminal() {
                            terminals.push((Terminal::Directive(instance), child.origin.clone()));
                        } else {
                            directives.push(instance);
                        }
                    }
                }
            }
        }

        let terminal = match terminals.len() {
            1 => terminals.pop().map(|(terminal, _)| terminal)?,
            0 => {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::TerminalArity,
                        node.origin.clone(),
                        format!("route `{raw_path}` has nothing to answer with"),
                    )
                    .with_help("add a terminal such as `proxy`, `respond` or `file-server`"),
                );
                return None;
            }
            _ => {
                let names: Vec<&str> = terminals.iter().map(|(terminal, _)| terminal.name()).collect();
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::TerminalArity,
                        terminals[1].1.clone(),
                        format!(
                            "route `{raw_path}` has more than one terminal: {}",
                            names.join(", ")
                        ),
                    )
                    .with_related("first terminal here", terminals[0].1.clone())
                    .with_help("a route answers in exactly one way; split it into two routes"),
                );
                return None;
            }
        };

        let id = match explicit_id {
            Some(name) => {
                if !owner.owns(&name) {
                    self.report_ownership(owner, &name, &node.origin);
                    return None;
                }
                ResourceId::new(ResourceKind::Route, name)
            }
            None => ResourceId::derived(
                ResourceKind::Route,
                &[
                    site.name(),
                    &host.describe(),
                    &methods.describe(),
                    &path.describe(),
                ],
            ),
        };

        reject_duplicate_directives(&directives, self.diagnostics);
        directives.sort_by_key(crate::directive::DirectiveInstance::sort_key);

        if self.diagnostics.error_count() > errors_before {
            return None;
        }
        Some(Route {
            id,
            path,
            methods,
            matchers,
            directives,
            terminal,
            origin: node.origin.clone(),
        })
    }

    /// `proxy` in its three forms: an address, a named upstream, or an inline
    /// upstream body. All three end up as an `Upstream` resource, so there is
    /// only one code path downstream.
    fn proxy(
        &mut self,
        node: &Node,
        site: &ResourceId,
        owner: &SourceId,
        upstreams: &mut Vec<Upstream>,
    ) -> Option<Terminal> {
        let named = node.prop("upstream");
        let inline_address = node.args.first();

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
        let id = anonymous_upstream_id(site, &addresses);
        let upstream = decode_upstream_body(&body, id.clone(), owner.clone(), self.diagnostics)?;

        // Two routes proxying to the same address share one upstream, and so
        // share its pool and health state instead of competing.
        if !upstreams.iter().any(|existing| existing.id == id) {
            upstreams.push(upstream);
        }
        Some(Terminal::Proxy { upstream: id })
    }
}

fn reject_duplicate_directives(
    directives: &[crate::directive::DirectiveInstance],
    diagnostics: &mut Diagnostics,
) {
    let mut seen: BTreeMap<&str, &Origin> = BTreeMap::new();
    for instance in directives {
        match seen.get(instance.meta.name) {
            Some(first) => diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    instance.origin.clone(),
                    format!("`{}` is declared twice in the same scope", instance.meta.name),
                )
                .with_related("first declared here", Origin::clone(first))
                .with_help("combine them, or move one to the other scope"),
            ),
            None => {
                seen.insert(instance.meta.name, &instance.origin);
            }
        }
    }
}

fn check_uniqueness(spec: &Spec, diagnostics: &mut Diagnostics) {
    let mut listener_names: BTreeMap<&str, &Origin> = BTreeMap::new();
    let mut binds: BTreeMap<SocketAddr, &Origin> = BTreeMap::new();
    for listener in &spec.listeners {
        if let Some(first) = listener_names.insert(&listener.name, &listener.origin) {
            diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    listener.origin.clone(),
                    format!("listener `{}` is declared twice", listener.name),
                )
                .with_related("first declared here", first.clone()),
            );
        }
        if let Some(first) = binds.insert(listener.bind, &listener.origin) {
            diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    listener.origin.clone(),
                    format!("two listeners bind {}", listener.bind),
                )
                .with_related("first declared here", first.clone()),
            );
        }
    }

    let mut upstream_ids: BTreeMap<&str, &Origin> = BTreeMap::new();
    for upstream in &spec.upstreams {
        if let Some(first) = upstream_ids.insert(upstream.id.name(), &upstream.origin) {
            diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    upstream.origin.clone(),
                    format!("upstream `{}` is declared twice", upstream.id.name()),
                )
                .with_related("first declared here", first.clone()),
            );
        }
    }

    // Two routes that could both claim a request are ambiguous: the winner
    // would depend on declaration order, which must never decide behaviour.
    for site in &spec.sites {
        let mut seen: BTreeMap<String, &Origin> = BTreeMap::new();
        for route in &site.routes {
            let key = format!(
                "{}|{}|{}|{}",
                route.path.describe(),
                route.methods.describe(),
                route
                    .matchers
                    .iter()
                    .map(crate::matcher::Matcher::describe)
                    .collect::<Vec<_>>()
                    .join(","),
                site.host.describe()
            );
            if let Some(first) = seen.insert(key, &route.origin) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::Duplicate,
                        route.origin.clone(),
                        format!(
                            "two routes in site `{}` match exactly the same requests",
                            site.host.describe()
                        ),
                    )
                    .with_related("first declared here", first.clone())
                    .with_help("add a `match` condition, or merge them"),
                );
            }
        }
    }
}

fn check_references(spec: &Spec, diagnostics: &mut Diagnostics) {
    let listener_names: Vec<&str> = spec
        .listeners
        .iter()
        .map(|listener| listener.name.as_str())
        .collect();

    for site in &spec.sites {
        for name in &site.listeners {
            if !listener_names.contains(&name.as_str()) {
                let mut diagnostic = Diagnostic::error(
                    Code::UnresolvedReference,
                    site.origin.clone(),
                    format!(
                        "site `{}` refers to unknown listener `{name}`",
                        site.host.describe()
                    ),
                );
                if let Some(suggestion) = did_you_mean(name, listener_names.iter().copied()) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                } else {
                    diagnostic =
                        diagnostic.with_help(format!("declared listeners: {}", listener_names.join(", ")));
                }
                diagnostics.push(diagnostic);
            }
        }
        if site.tls.is_some() {
            let served = spec.listeners_for(site);
            if !served.is_empty() && served.iter().all(|listener| !listener.protocol.is_tls()) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        site.origin.clone(),
                        format!(
                            "site `{}` configures TLS but is only served on plaintext listeners",
                            site.host.describe()
                        ),
                    )
                    .with_help("add an `https` listener, or point the site at one"),
                );
            }
        }
        if spec.listeners_for(site).is_empty() {
            diagnostics.push(
                Diagnostic::error(
                    Code::UnresolvedReference,
                    site.origin.clone(),
                    format!("site `{}` is not served on any listener", site.host.describe()),
                )
                .with_help(if site.tls.is_some() {
                    "a site with `tls` needs an `https` listener"
                } else {
                    "a site without `tls` needs an `http` listener"
                }),
            );
        }

        for route in &site.routes {
            if let Terminal::Proxy { upstream } = &route.terminal
                && spec.upstream(upstream).is_none()
            {
                let names: Vec<&str> = spec
                    .upstreams
                    .iter()
                    .map(|candidate| candidate.id.name())
                    .collect();
                let mut diagnostic = Diagnostic::error(
                    Code::UnresolvedReference,
                    route.origin.clone(),
                    format!("unknown upstream `{}`", upstream.name()),
                );
                if let Some(suggestion) = did_you_mean(upstream.name(), names.iter().copied()) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                } else if names.is_empty() {
                    diagnostic = diagnostic.with_help("no upstreams are declared; use `proxy \"host:port\"`");
                }
                diagnostics.push(diagnostic);
            }
        }
    }
}

/// Order routes so the most specific wins, once, at compile time. The request
/// path then walks a fixed list instead of scoring candidates.
fn sort_routes(spec: &mut Spec) {
    for site in &mut spec.sites {
        site.routes.sort_by(|a, b| {
            compare_specificity(
                (&site.host, &a.path, &a.methods, a.matchers.len()),
                (&site.host, &b.path, &b.methods, b.matchers.len()),
            )
            .then_with(|| a.id.name().cmp(b.id.name()))
        });
    }
    spec.sites.sort_by(|a, b| {
        b.host
            .specificity()
            .cmp(&a.host.specificity())
            .then_with(|| a.id.name().cmp(b.id.name()))
    });
    spec.listeners.sort_by(|a, b| a.name.cmp(&b.name));
    spec.upstreams.sort_by(|a, b| a.id.name().cmp(b.id.name()));
}
