//! Spec to plan.
//!
//! Compilation resolves everything that would otherwise have to be decided per
//! request: which sites a listener serves, in what order routes are tried, what
//! the middleware chain is, and which pieces of mutable state each route needs.
//!
//! The plan's fingerprint answers a different question from the spec's: not
//! "did the operator ask for something different?" but "would this behave
//! differently?". A binary built without the `cache` bundle produces a
//! different plan from the same spec, and must not be mistaken for it.

use std::sync::Arc;

use gatel_model::{Fingerprint, Hasher, ResourceId};

use crate::directive::{DirectiveInstance, Registry};
use crate::matcher::{HostMatch, Matcher, MethodMatch, PathMatch};
use crate::spec::{Listener, Site, SiteTls, Spec, Terminal};
use crate::upstream::Upstream;

/// A kind of state that outlives the generation that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StateKind {
    /// The set of backends and where they came from.
    Endpoints,
    /// Health, both probed and observed.
    Health,
    /// Live connections to backends.
    Pool,
    /// A balancer's cursor or counters.
    Selector,
    /// Token buckets.
    RateLimit,
    /// Breaker counters and trip state.
    CircuitBreaker,
    /// Loaded and issued certificates.
    Certificate,
}

impl StateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            StateKind::Endpoints => "endpoints",
            StateKind::Health => "health",
            StateKind::Pool => "pool",
            StateKind::Selector => "selector",
            StateKind::RateLimit => "rate-limit",
            StateKind::CircuitBreaker => "circuit-breaker",
            StateKind::Certificate => "certificate",
        }
    }
}

/// What happens to existing state when a new generation needs the same slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetPolicy {
    /// Keep it if the compatibility fingerprint matches.
    Preserve,
    /// Always start fresh; drain the old instance.
    Reset,
}

/// One piece of state a plan needs, named so it can be matched against what is
/// already running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateBinding {
    pub owner: ResourceId,
    pub kind: StateKind,
    /// Changes exactly when the existing instance can no longer be reused.
    /// Growing a pool's size limit does not change it; changing the endpoint
    /// set or the upstream's TLS identity does.
    pub compatibility: Fingerprint,
    pub policy: ResetPolicy,
}

impl StateBinding {
    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.owner.hash_into(hasher);
        hasher.update_str(self.kind.as_str());
        hasher.update_fingerprint(&self.compatibility);
        hasher.update_bool(self.policy == ResetPolicy::Reset);
    }
}

/// A route, with its middleware chain already flattened and ordered.
#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub id: ResourceId,
    pub path: PathMatch,
    pub methods: MethodMatch,
    pub matchers: Vec<Matcher>,
    /// Site directives and route directives merged into one ordered chain.
    pub chain: Vec<DirectiveInstance>,
    pub terminal: Terminal,
}

impl RoutePlan {
    /// Whether this route could serve the given request line. Routes are
    /// pre-sorted, so the first match wins and nothing is scored at runtime.
    pub fn accepts(&self, path: &str, method: &str) -> bool {
        self.path.matches(path) && self.methods.matches(method)
    }
}

#[derive(Debug, Clone)]
pub struct SitePlan {
    pub id: ResourceId,
    pub host: HostMatch,
    pub tls: Option<SiteTls>,
    pub routes: Vec<RoutePlan>,
    /// The site's own directives, ordered, for a request that reached this site
    /// and matched none of its routes.
    ///
    /// Without this, a site-wide `access-log` would miss every request to an
    /// unknown path — the ones most worth seeing — and a site-wide
    /// `error-pages` could not cover its own 404.
    pub fallback: Vec<DirectiveInstance>,
}

#[derive(Debug, Clone)]
pub struct ListenerPlan {
    pub listener: Listener,
    /// Sites this listener serves, most specific host first.
    pub sites: Vec<SitePlan>,
}

impl ListenerPlan {
    /// The site that claims a host, or none.
    pub fn site_for(&self, host: &str) -> Option<&SitePlan> {
        self.sites.iter().find(|site| site.host.matches(host))
    }
}

/// An immutable execution plan.
#[derive(Debug, Clone)]
pub struct Plan {
    pub spec: Arc<Spec>,
    pub listeners: Vec<ListenerPlan>,
    pub upstreams: Vec<Arc<Upstream>>,
    pub bindings: Vec<StateBinding>,
    spec_hash: Fingerprint,
    plan_hash: Fingerprint,
}

impl Plan {
    pub fn spec_hash(&self) -> Fingerprint {
        self.spec_hash
    }

    pub fn plan_hash(&self) -> Fingerprint {
        self.plan_hash
    }

    pub fn upstream(&self, id: &ResourceId) -> Option<&Arc<Upstream>> {
        self.upstreams.iter().find(|upstream| &upstream.id == id)
    }

    pub fn listener(&self, name: &str) -> Option<&ListenerPlan> {
        self.listeners.iter().find(|plan| plan.listener.name == name)
    }

    pub fn route_count(&self) -> usize {
        self.listeners
            .iter()
            .flat_map(|listener| listener.sites.iter())
            .map(|site| site.routes.len())
            .sum()
    }
}

/// Compile a spec against the directives this binary actually contains.
///
/// This cannot fail on configuration grounds: normalization already rejected
/// anything invalid. It is a pure rearrangement plus fingerprinting.
pub fn compile(spec: Arc<Spec>, registry: &Registry) -> Plan {
    let mut listeners = Vec::with_capacity(spec.listeners.len());

    for listener in &spec.listeners {
        let mut sites: Vec<SitePlan> = spec
            .sites
            .iter()
            .filter(|site| {
                spec.listeners_for(site)
                    .iter()
                    .any(|candidate| candidate.name == listener.name)
            })
            .map(compile_site)
            .collect();
        // Most specific host first, so the request path takes the first match.
        sites.sort_by(|a, b| {
            b.host
                .specificity()
                .cmp(&a.host.specificity())
                .then_with(|| a.id.name().cmp(b.id.name()))
        });
        listeners.push(ListenerPlan {
            listener: listener.clone(),
            sites,
        });
    }

    let upstreams: Vec<Arc<Upstream>> = spec.upstreams.iter().cloned().map(Arc::new).collect();
    let bindings = collect_bindings(&spec, &upstreams);

    let spec_hash = spec.fingerprint();
    let mut hasher = Hasher::new();
    hasher.update_str("gatel.plan.v1");
    hasher.update_fingerprint(&spec_hash);
    // A differently-trimmed binary compiles the same spec into different
    // behaviour, so the build takes part in the plan's identity.
    hasher.update_fingerprint(&registry.build_hash());
    hasher.update_ordered(&bindings, |binding, hasher| binding.hash_into(hasher));
    let plan_hash = hasher.finish();

    Plan {
        spec,
        listeners,
        upstreams,
        bindings,
        spec_hash,
        plan_hash,
    }
}

fn compile_site(site: &Site) -> SitePlan {
    let routes = site
        .routes
        .iter()
        .map(|route| {
            // Site-scoped directives run for every route, so they are merged in
            // here once rather than consulted per request.
            let mut chain = site.directives.clone();
            chain.extend(route.directives.iter().cloned());
            chain.sort_by_key(DirectiveInstance::sort_key);
            RoutePlan {
                id: route.id.clone(),
                path: route.path.clone(),
                methods: route.methods.clone(),
                matchers: route.matchers.clone(),
                chain,
                terminal: route.terminal.clone(),
            }
        })
        .collect();
    let mut fallback = site.directives.clone();
    fallback.sort_by_key(DirectiveInstance::sort_key);
    SitePlan {
        id: site.id.clone(),
        host: site.host.clone(),
        tls: site.tls.clone(),
        routes,
        fallback,
    }
}

fn collect_bindings(spec: &Spec, upstreams: &[Arc<Upstream>]) -> Vec<StateBinding> {
    let mut bindings = Vec::new();

    for upstream in upstreams {
        // The endpoint set and the transport define whether existing
        // connections may still be used. Pool sizing does not: raising a limit
        // must not tear down live connections.
        let mut transport = Hasher::new();
        transport.update_unordered(&upstream.endpoints, |endpoint, hasher| {
            hasher.update_str(&endpoint.id);
            hasher.update_str(&endpoint.address.to_string());
        });
        transport.update_u64(upstream.protocol as u64);
        match &upstream.tls {
            Some(tls) => tls.hash_into(&mut transport),
            None => {
                transport.update_none();
            }
        }
        let transport = transport.finish();

        bindings.push(StateBinding {
            owner: upstream.id.clone(),
            kind: StateKind::Endpoints,
            compatibility: transport,
            policy: ResetPolicy::Preserve,
        });
        bindings.push(StateBinding {
            owner: upstream.id.clone(),
            kind: StateKind::Pool,
            compatibility: transport,
            policy: ResetPolicy::Preserve,
        });

        // A balancer's cursor is only meaningful for the strategy that wrote
        // it, so changing strategy starts a new one.
        let mut selector = Hasher::new();
        upstream.balance.hash_into(&mut selector);
        selector.update_fingerprint(&transport);
        bindings.push(StateBinding {
            owner: upstream.id.clone(),
            kind: StateKind::Selector,
            compatibility: selector.finish(),
            policy: ResetPolicy::Preserve,
        });

        if upstream.health.is_some() || upstream.passive_health.is_some() {
            let mut health = Hasher::new();
            match &upstream.health {
                Some(probe) => probe.hash_into(&mut health),
                None => {
                    health.update_none();
                }
            }
            match &upstream.passive_health {
                Some(passive) => passive.hash_into(&mut health),
                None => {
                    health.update_none();
                }
            }
            health.update_fingerprint(&transport);
            bindings.push(StateBinding {
                owner: upstream.id.clone(),
                kind: StateKind::Health,
                compatibility: health.finish(),
                policy: ResetPolicy::Preserve,
            });
        }
    }

    // A directive that keeps something between requests declares it, and it
    // lands in the same table the pool and the balancer use.
    for site in &spec.sites {
        for directive in &site.directives {
            if let Some(binding) = directive.state_binding() {
                bindings.push(binding);
            }
        }
        for route in &site.routes {
            for directive in &route.directives {
                if let Some(binding) = directive.state_binding() {
                    bindings.push(binding);
                }
            }
            if let Terminal::Directive(instance) = &route.terminal
                && let Some(binding) = instance.state_binding()
            {
                bindings.push(binding);
            }
        }
    }

    for site in &spec.sites {
        if let Some(tls) = &site.tls {
            let mut certificate = Hasher::new();
            tls.hash_into(&mut certificate);
            bindings.push(StateBinding {
                owner: site.id.clone(),
                kind: StateKind::Certificate,
                compatibility: certificate.finish(),
                // A certificate outlives configuration: renewing must not
                // reset it, and reconfiguring the site must not lose it.
                policy: ResetPolicy::Preserve,
            });
        }
    }

    bindings.sort_by(|a, b| {
        a.owner
            .name()
            .cmp(b.owner.name())
            .then_with(|| a.kind.cmp(&b.kind))
    });
    bindings
}

/// How a plan's state bindings relate to what is already running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingChange {
    /// Reusable as is.
    Reuse(StateBinding),
    /// A new instance is needed; the old one drains.
    Replace {
        binding: StateBinding,
        reason: &'static str,
    },
    /// Nothing was running for this slot.
    Create(StateBinding),
    /// Nothing in the new plan needs it; it drains.
    Retire(StateBinding),
}

/// Decide, for every binding, whether existing state carries over.
pub fn diff_bindings(current: &[StateBinding], next: &[StateBinding]) -> Vec<BindingChange> {
    let mut changes = Vec::new();
    for binding in next {
        let existing = current
            .iter()
            .find(|candidate| candidate.owner == binding.owner && candidate.kind == binding.kind);
        changes.push(match existing {
            None => BindingChange::Create(binding.clone()),
            Some(_) if binding.policy == ResetPolicy::Reset => BindingChange::Replace {
                binding: binding.clone(),
                reason: "reset policy",
            },
            Some(existing) if existing.compatibility != binding.compatibility => BindingChange::Replace {
                binding: binding.clone(),
                reason: "incompatible change",
            },
            Some(_) => BindingChange::Reuse(binding.clone()),
        });
    }
    for binding in current {
        let still_needed = next
            .iter()
            .any(|candidate| candidate.owner == binding.owner && candidate.kind == binding.kind);
        if !still_needed {
            changes.push(BindingChange::Retire(binding.clone()));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use gatel_model::{Diagnostics, Node, Origin, ResourceKind, SourceId};
    use salvo_core::Handler;

    use super::*;
    use crate::directive::{Bundle, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};

    struct Marker(DirectiveMeta);
    struct MarkerConfig(String);

    impl DirectiveConfig for MarkerConfig {
        fn hash_into(&self, hasher: &mut Hasher) {
            hasher.update_str(&self.0);
        }

        fn build(&self, _cx: crate::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
            Err("test".into())
        }

        fn describe(&self) -> String {
            self.0.clone()
        }
    }

    impl Directive for Marker {
        fn meta(&self) -> &DirectiveMeta {
            &self.0
        }

        fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
            Some(Arc::new(MarkerConfig(node.name.clone())))
        }
    }

    fn registry() -> Registry {
        Registry::builder()
            .bundle(
                Bundle::new("test", "1")
                    .with(Marker(DirectiveMeta::middleware(
                        "headers",
                        Phase::Response,
                        Scopes::BOTH,
                    )))
                    .with(Marker(DirectiveMeta::terminal("respond"))),
            )
            .build()
            .unwrap()
    }

    fn upstream(name: &str, addresses: &[&str], balance: crate::upstream::Balance) -> Upstream {
        Upstream {
            id: ResourceId::new(ResourceKind::Upstream, name),
            owner: SourceId::file("main"),
            endpoints: addresses
                .iter()
                .map(|address| crate::upstream::Endpoint {
                    id: (*address).to_string(),
                    address: address.parse().unwrap(),
                    weight: 1,
                    priority: 0,
                    origin: Origin::synthetic("test"),
                })
                .collect(),
            balance,
            timeouts: Default::default(),
            pool: Default::default(),
            retry: None,
            health: None,
            passive_health: None,
            protocol: Default::default(),
            tls: None,
            origin: Origin::synthetic("test"),
        }
    }

    fn spec_with(upstreams: Vec<Upstream>) -> Arc<Spec> {
        Arc::new(Spec {
            global: Default::default(),
            listeners: vec![],
            sites: vec![],
            upstreams,
        })
    }

    #[test]
    fn every_upstream_binds_endpoints_pool_and_selector() {
        let plan = compile(
            spec_with(vec![upstream("api", &["127.0.0.1:1"], Default::default())]),
            &registry(),
        );
        let kinds: Vec<StateKind> = plan.bindings.iter().map(|binding| binding.kind).collect();
        assert!(kinds.contains(&StateKind::Endpoints));
        assert!(kinds.contains(&StateKind::Pool));
        assert!(kinds.contains(&StateKind::Selector));
        assert!(
            !kinds.contains(&StateKind::Health),
            "health is only bound when configured"
        );
    }

    #[test]
    fn resizing_a_pool_keeps_existing_connections() {
        let registry = registry();
        let mut small = upstream("api", &["127.0.0.1:1"], Default::default());
        let before = compile(spec_with(vec![small.clone()]), &registry);
        small.pool.max_per_endpoint = 128;
        let after = compile(spec_with(vec![small]), &registry);

        let changes = diff_bindings(&before.bindings, &after.bindings);
        let pool = changes.iter().find(
            |change| matches!(change, BindingChange::Reuse(binding) if binding.kind == StateKind::Pool),
        );
        assert!(
            pool.is_some(),
            "raising a pool limit must not drop connections: {changes:?}"
        );
    }

    #[test]
    fn changing_the_endpoint_set_replaces_pool_and_endpoints() {
        let registry = registry();
        let before = compile(
            spec_with(vec![upstream("api", &["127.0.0.1:1"], Default::default())]),
            &registry,
        );
        let after = compile(
            spec_with(vec![upstream("api", &["127.0.0.1:2"], Default::default())]),
            &registry,
        );
        let changes = diff_bindings(&before.bindings, &after.bindings);
        assert!(
            changes
                .iter()
                .filter(|change| matches!(change, BindingChange::Replace { .. }))
                .count()
                >= 2
        );
    }

    #[test]
    fn changing_the_balance_strategy_only_replaces_the_selector() {
        let registry = registry();
        let before = compile(
            spec_with(vec![upstream(
                "api",
                &["127.0.0.1:1"],
                crate::upstream::Balance::RoundRobin,
            )]),
            &registry,
        );
        let after = compile(
            spec_with(vec![upstream(
                "api",
                &["127.0.0.1:1"],
                crate::upstream::Balance::LeastConn,
            )]),
            &registry,
        );
        let changes = diff_bindings(&before.bindings, &after.bindings);
        let replaced: Vec<StateKind> = changes
            .iter()
            .filter_map(|change| match change {
                BindingChange::Replace { binding, .. } => Some(binding.kind),
                _ => None,
            })
            .collect();
        assert_eq!(replaced, vec![StateKind::Selector]);
    }

    #[test]
    fn removing_an_upstream_retires_its_state() {
        let registry = registry();
        let before = compile(
            spec_with(vec![upstream("api", &["127.0.0.1:1"], Default::default())]),
            &registry,
        );
        let after = compile(spec_with(vec![]), &registry);
        let changes = diff_bindings(&before.bindings, &after.bindings);
        assert!(
            changes
                .iter()
                .all(|change| matches!(change, BindingChange::Retire(_)))
        );
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn the_plan_hash_reflects_the_build_not_only_the_spec() {
        let spec = spec_with(vec![upstream("api", &["127.0.0.1:1"], Default::default())]);
        let full = compile(Arc::clone(&spec), &registry());
        let trimmed = compile(
            Arc::clone(&spec),
            &Registry::builder()
                .bundle(Bundle::new("test", "1").with(Marker(DirectiveMeta::terminal("respond"))))
                .build()
                .unwrap(),
        );
        assert_eq!(full.spec_hash(), trimmed.spec_hash());
        assert_ne!(full.plan_hash(), trimmed.plan_hash());
    }
}
