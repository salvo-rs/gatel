//! Checks that need the whole configuration.
//!
//! Everything here is about a pair of resources rather than one: two listeners
//! on one address, two routes that would claim the same request, a reference
//! that resolves to nothing. None of it can be decided while decoding a single
//! node, which is why it runs after the graph is assembled.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use gatel_model::{Code, Diagnostic, Diagnostics, Origin, did_you_mean};

use crate::matcher::{HostMatch, compare_specificity};
use crate::spec::{CertificateSource, SiteTls, Spec, Terminal};

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
    let mut site_ids = BTreeMap::new();
    let mut route_ids = BTreeMap::new();
    for site in &spec.sites {
        if let Some(first) = site_ids.insert(&site.id, &site.origin) {
            diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    site.origin.clone(),
                    "site resource ID is declared more than once",
                )
                .with_related("first declared here", first.clone()),
            );
        }
        for route in &site.routes {
            if let Some(first) = route_ids.insert(&route.id, &route.origin) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::Duplicate,
                        route.origin.clone(),
                        "route resource ID is declared more than once across the configuration",
                    )
                    .with_related("first declared here", first.clone()),
                );
            }
        }
    }
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
    check_acme(spec, diagnostics);
    check_proxy_protocol(spec, diagnostics);
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

        for directive in &site.directives {
            check_directive_upstreams(spec, directive, diagnostics);
        }

        for route in &site.routes {
            for directive in &route.directives {
                check_directive_upstreams(spec, directive, diagnostics);
            }
            if let Terminal::Proxy { upstream } = &route.terminal
                && spec.upstream(upstream).is_none()
            {
                diagnostics.push(unknown_upstream(spec, upstream, route.origin.clone()));
            }
        }
    }
}

/// An upstream a directive named but the file does not declare.
///
/// The proxy terminal is not the only thing that reaches a backend: anything
/// that has to ask one a question names it the same way, and gets the same
/// answer to a name that is not there.
fn check_directive_upstreams(
    spec: &Spec,
    directive: &crate::directive::DirectiveInstance,
    diagnostics: &mut Diagnostics,
) {
    for upstream in directive.config.upstreams() {
        if spec.upstream(&upstream).is_none() {
            diagnostics.push(unknown_upstream(spec, &upstream, directive.origin.clone()));
        }
    }
}

fn unknown_upstream(spec: &Spec, upstream: &gatel_model::ResourceId, origin: Origin) -> Diagnostic {
    let names: Vec<&str> = spec
        .upstreams
        .iter()
        .map(|candidate| candidate.id.name())
        .collect();
    let mut diagnostic = Diagnostic::error(
        Code::UnresolvedReference,
        origin,
        format!("unknown upstream `{}`", upstream.name()),
    );
    if let Some(suggestion) = did_you_mean(upstream.name(), names.iter().copied()) {
        diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
    } else if names.is_empty() {
        diagnostic = diagnostic.with_help("no upstreams are declared; use `proxy \"host:port\"`");
    }
    diagnostic
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

/// A listener that reads PROXY headers has to know whose it will read.
///
/// The header lets whoever sends it name any client address. That is safe only
/// when the port is reachable by a balancer and nobody else, which is a fact
/// the configuration has to state — otherwise every ip-based decision in the
/// gateway is handed to whoever connects. Found here rather than at the first
/// connection, because the failure is silent: everything works, and the
/// addresses are wrong.
fn check_proxy_protocol(spec: &Spec, diagnostics: &mut Diagnostics) {
    if spec.global.trusted_proxies.is_empty() {
        for listener in spec.listeners.iter().filter(|l| l.proxy_protocol) {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    listener.origin.clone(),
                    format!(
                        "listener `{}` reads PROXY headers, but no `trusted-proxy` says whose",
                        listener.name
                    ),
                )
                .with_help("add `trusted-proxy \"10.0.0.0/8\"` to `global`, naming the balancer's network"),
            );
        }
    }
}

/// A site that asks for an automatic certificate has to be able to get one.
///
/// Both of these are found here rather than at issuance time, because the first
/// time anyone would otherwise notice is a handshake failing in production.
fn check_acme(spec: &Spec, diagnostics: &mut Diagnostics) {
    for site in &spec.sites {
        let Some(tls) = &site.tls else { continue };
        if tls.source != CertificateSource::Acme {
            continue;
        }
        if spec.global.acme.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    Code::UnresolvedReference,
                    site.origin.clone(),
                    format!(
                        "site `{}` asks for `tls acme`, but nothing configured a certificate authority",
                        site.host.describe()
                    ),
                )
                .with_help("add `acme email=\"...\" agreed=#true` to `global`"),
            );
        }
        // A CA issues for names, and a wildcard site does not name one. The
        // wildcard certificate a DNS-01 challenge could get is a different
        // thing, and would have to be asked for explicitly.
        if !matches!(site.host, HostMatch::Exact(_)) {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    site.origin.clone(),
                    format!(
                        "site `{}` cannot use `tls acme`: there is no single host to certify",
                        site.host.describe()
                    ),
                )
                .with_help("name the site, or use `tls cert=.. key=..` for a certificate you already have"),
            );
        }
    }
}

/// Give a local name a development certificate when it would otherwise have
/// nowhere to be served.
///
/// This is the one place the compiler fills something in that the operator did
/// not write, and it is narrow on purpose. It applies only when every listener
/// the site could use terminates TLS — so without a certificate the site would
/// be served nowhere at all — and only to a name no public authority would ever
/// issue for. It turns an error into the only outcome that could have been
/// meant; it never moves a site that was already working.
///
/// The point is what a test environment feels like. `https ":8443"` and
/// `site "localhost"` should serve, once `gatel trust` has been run, without a
/// line about certificates that has nothing to do with what is being tested.
pub(super) fn apply_automatic_tls(spec: &mut Spec, diagnostics: &mut Diagnostics) {
    let tls_listeners: Vec<String> = spec
        .listeners
        .iter()
        .filter(|listener| listener.protocol.is_tls())
        .map(|listener| listener.name.clone())
        .collect();
    let plaintext_listeners: Vec<String> = spec
        .listeners
        .iter()
        .filter(|listener| !listener.protocol.is_tls())
        .map(|listener| listener.name.clone())
        .collect();
    if tls_listeners.is_empty() {
        return;
    }

    for site in &mut spec.sites {
        if site.tls.is_some() {
            continue;
        }
        // Could this site be served in plaintext? If so it already has
        // somewhere to go, and nothing is inferred.
        let has_plaintext = if site.listeners.is_empty() {
            !plaintext_listeners.is_empty()
        } else {
            site.listeners
                .iter()
                .any(|name| plaintext_listeners.contains(name))
        };
        if has_plaintext {
            continue;
        }
        let on_tls = if site.listeners.is_empty() {
            true
        } else {
            site.listeners.iter().any(|name| tls_listeners.contains(name))
        };
        if !on_tls {
            continue;
        }

        match site.host.local_name() {
            Some(_) => {
                site.tls = Some(SiteTls {
                    source: CertificateSource::Internal,
                    client_auth: None,
                    automatic: true,
                });
            }
            // A name somebody else has to vouch for. Saying which of the three
            // ways to get one is missing beats inventing a certificate that no
            // client would accept.
            None => diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    site.origin.clone(),
                    format!(
                        "site `{}` can only be served over TLS, and has no certificate",
                        site.host.describe()
                    ),
                )
                .with_help(
                    "add `tls cert=.. key=..` for one you have, `tls acme` to have one issued,                      or `tls internal` for a development certificate",
                ),
            ),
        }
    }
}
