//! Checks that need the whole configuration.
//!
//! Everything here is about a pair of resources rather than one: two listeners
//! on one address, two routes that would claim the same request, a reference
//! that resolves to nothing. None of it can be decided while decoding a single
//! node, which is why it runs after the graph is assembled.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use gatel_model::{Code, Diagnostic, Diagnostics, Origin, did_you_mean};

use crate::matcher::compare_specificity;
use crate::spec::{Spec, Terminal};

pub(super) fn reject_duplicate_directives(
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

pub(super) fn check_uniqueness(spec: &Spec, diagnostics: &mut Diagnostics) {
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

pub(super) fn check_references(spec: &Spec, diagnostics: &mut Diagnostics) {
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
pub(super) fn sort_routes(spec: &mut Spec) {
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
