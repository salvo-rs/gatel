//! Snapshot to spec.
//!
//! This is where a configuration stops being text and becomes a resource graph:
//! names are resolved, identities are assigned, directives are looked up in the
//! registry and sorted into their phases, and every remaining ambiguity is
//! reported rather than guessed at.

use gatel_model::{
    Code, Diagnostic, Diagnostics, Node, Origin, ResourceId, ResourceKind, Snapshot, SourceId, did_you_mean,
};

use crate::directive::{Registry, Scope};
use crate::matcher::{HostMatch, MethodMatch, PathMatch, parse_match_node};
use crate::spec::{Global, Listener, Route, Site, Spec, Terminal};
use crate::upstream::Upstream;

mod global;
mod proxy;
mod tls;
mod validate;

use validate::{
    apply_automatic_tls, check_references, check_uniqueness, reject_duplicate_directives, sort_routes,
};

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
        // Namespaced runtime sources may contribute resources, not replace
        // process-wide listeners, limits, logging or certificate authority.
        if node.name == "global" && owner.kind() != gatel_model::SourceKind::File {
            context.diagnostics.error(
                Code::OwnershipConflict,
                node.origin.clone(),
                "only file sources may declare global host configuration",
            );
            continue;
        }
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
    // Before every check that reads `tls`, because what it fills in is part of
    // what the rest of them are checking.
    apply_automatic_tls(&mut spec, &mut diagnostics);
    check_uniqueness(&spec, &mut diagnostics);
    check_references(&spec, &mut diagnostics);
    sort_routes(&mut spec);
    for site in &spec.sites {
        if let Err(errors) = crate::ordering::order_chain(site.directives.clone()) {
            for error in errors.iter() {
                diagnostics.push(error.clone());
            }
        }
        for route in &site.routes {
            let mut chain = site.directives.clone();
            chain.extend(route.directives.iter().cloned());
            if let Err(errors) = crate::ordering::order_route(chain, &route.terminal) {
                for error in errors.iter() {
                    diagnostics.push(error.clone());
                }
            }
        }
    }

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

impl Context<'_> {
    pub(super) fn error(&mut self, code: Code, origin: &Origin, message: impl Into<String>) {
        self.diagnostics.error(code, origin.clone(), message);
    }

    pub(super) fn report_ownership(&mut self, owner: &SourceId, name: &str, origin: &Origin) {
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
                ResourceId::derived_owned(ResourceKind::Site, owner, &refs)
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

        // Only when nothing else already explained why. A site whose only
        // route was refused has one problem, not two, and the reader should be
        // looking at the first.
        if routes.is_empty() && self.diagnostics.error_count() == errors_before {
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
        // Now that the site has an identity, its directives can own state under
        // it: state follows the resource it was written on.
        for directive in &mut directives {
            directive.own(&id);
        }

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

        let mut terminal = match terminals.len() {
            1 => terminals.pop().map(|(terminal, _)| terminal)?,
            // A terminal that failed to decode already said why, and saying
            // there is none on top of that buries the reason.
            0 if self.diagnostics.error_count() > errors_before => return None,
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
            None => {
                let mut parts = vec![
                    site.name().to_string(),
                    host.describe(),
                    methods.describe(),
                    path.describe(),
                ];
                if !matchers.is_empty() {
                    let mut hash = gatel_model::Hasher::new();
                    hash.update_str("route-matchers-v1");
                    hash.update_unordered(&matchers, |matcher, hash| matcher.hash_into(hash));
                    parts.push(hash.finish().to_string());
                }
                let refs: Vec<_> = parts.iter().map(String::as_str).collect();
                ResourceId::derived_owned(ResourceKind::Route, owner, &refs)
            }
        };

        reject_duplicate_directives(&directives, self.diagnostics);
        directives.sort_by_key(crate::directive::DirectiveInstance::sort_key);
        for directive in &mut directives {
            directive.own(&id);
        }

        if let Terminal::Directive(instance) = &mut terminal {
            instance.own(&id);
        }

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
}
