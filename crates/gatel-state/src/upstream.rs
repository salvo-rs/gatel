//! One upstream's runtime: its endpoints, its balancer and its connections,
//! resolved from the plan's state bindings so a compatible reload keeps them.

use std::sync::Arc;

use gatel_model::ResourceId;
use gatel_plan::{Plan, StateBinding, StateKind, Upstream};

use crate::endpoint::EndpointSet;
use crate::pool::ConnectionPool;
use crate::registry::{Resolution, StateRegistry};
use crate::selector::SelectorState;

/// Everything a proxy terminal needs to reach one upstream.
#[derive(Debug)]
pub struct UpstreamRuntime {
    pub spec: Arc<Upstream>,
    pub endpoints: Arc<EndpointSet>,
    pub selector: Arc<SelectorState>,
    pub pool: Arc<ConnectionPool>,
}

/// How each part of an upstream's state was resolved, for the reload event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamResolution {
    pub endpoints: Resolution,
    pub selector: Resolution,
    pub pool: Resolution,
}

impl UpstreamResolution {
    pub fn all_reused(&self) -> bool {
        self.endpoints == Resolution::Reused
            && self.selector == Resolution::Reused
            && self.pool == Resolution::Reused
    }
}

/// Every upstream in a plan, ready to serve.
#[derive(Debug, Default)]
pub struct UpstreamRuntimes {
    runtimes: Vec<Arc<UpstreamRuntime>>,
}

impl UpstreamRuntimes {
    /// Resolve every upstream in a plan against what is already running.
    ///
    /// This is the point where "the operator raised a pool limit" and "the
    /// operator replaced the backends" become different things: the first keeps
    /// live connections, the second does not.
    /// `root` is the directory `file:` references resolve against, for the
    /// certificate authority and client identity an upstream may name.
    ///
    /// Returns an error only for something the configuration got wrong about
    /// TLS. Everything else here is a lookup against what is already running
    /// and cannot fail.
    pub fn resolve(
        plan: &Plan,
        state: &StateRegistry,
        root: &std::path::Path,
    ) -> Result<(Self, Vec<(ResourceId, UpstreamResolution)>), String> {
        let mut runtimes = Vec::with_capacity(plan.upstreams.len());
        let mut resolutions = Vec::with_capacity(plan.upstreams.len());

        for upstream in &plan.upstreams {
            let binding = |kind: StateKind| -> StateBinding {
                plan.bindings
                    .iter()
                    .find(|binding| binding.owner == upstream.id && binding.kind == kind)
                    .cloned()
                    .expect("compile binds endpoints, selector and pool for every upstream")
            };

            // Built before the pool is bound, so a bad authority stops the
            // reload rather than being discovered on the first request.
            let tls = match &upstream.tls {
                Some(spec) => Some(Arc::new(
                    gatel_tls::ClientTls::new(spec, upstream.protocol, root)
                        .map_err(|error| format!("upstream `{}`: {error}", upstream.id.name()))?,
                )),
                None => None,
            };

            let (endpoints, endpoints_resolution) =
                state.bind(&binding(StateKind::Endpoints), || EndpointSet::new(upstream));
            let (selector, selector_resolution) =
                state.bind(&binding(StateKind::Selector), SelectorState::new);
            let (pool, pool_resolution) = state.bind(&binding(StateKind::Pool), || {
                ConnectionPool::new(upstream.pool, upstream.protocol, upstream.resolve, tls)
            });

            resolutions.push((
                upstream.id.clone(),
                UpstreamResolution {
                    endpoints: endpoints_resolution,
                    selector: selector_resolution,
                    pool: pool_resolution,
                },
            ));
            runtimes.push(Arc::new(UpstreamRuntime {
                spec: Arc::clone(upstream),
                endpoints,
                selector,
                pool,
            }));
        }

        Ok((Self { runtimes }, resolutions))
    }

    pub fn get(&self, id: &ResourceId) -> Option<&Arc<UpstreamRuntime>> {
        self.runtimes.iter().find(|runtime| &runtime.spec.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<UpstreamRuntime>> {
        self.runtimes.iter()
    }

    pub fn len(&self) -> usize {
        self.runtimes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Diagnostics, Hasher, Node, Snapshot, SourceId, SourceRev, SourceSnapshot};
    use gatel_plan::{Bundle, Directive, DirectiveConfig, DirectiveMeta, Registry, compile, normalize};
    use salvo_core::Handler;

    use super::*;

    struct Respond(DirectiveMeta);
    struct RespondConfig;

    impl DirectiveConfig for RespondConfig {
        fn hash_into(&self, hasher: &mut Hasher) {
            hasher.update_str("respond");
        }

        fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
            Err("not built in tests".into())
        }

        fn describe(&self) -> String {
            "respond".into()
        }
    }

    impl Directive for Respond {
        fn meta(&self) -> &DirectiveMeta {
            &self.0
        }

        fn decode(&self, _: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
            Some(Arc::new(RespondConfig))
        }
    }

    fn registry() -> Registry {
        Registry::builder()
            .bundle(Bundle::new("std", "1").with(Respond(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap()
    }

    fn plan(text: &str) -> Plan {
        let nodes = gatel_kdl::lower(text, "gatel.kdl").expect("valid KDL");
        let mut hasher = Hasher::new();
        hasher.update_str(text);
        let snapshot = Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(1),
            hasher.finish(),
            nodes,
        ));
        let registry = registry();
        let spec = normalize(&snapshot, &registry).unwrap_or_else(|d| panic!("{d}"));
        compile(Arc::new(spec), &registry)
    }

    const BASE: &str = r#"
global { http ":8080" }
upstream "api" {
    endpoint "127.0.0.1:9000"
    pool max-per-endpoint=8
}
site "*" { route "/" { proxy upstream="api" } }
"#;

    #[test]
    fn a_compatible_reload_keeps_the_pool_and_the_cursor() {
        let state = StateRegistry::new();
        let (before, _) = UpstreamRuntimes::resolve(&plan(BASE), &state, std::path::Path::new(".")).unwrap();

        // Raising a limit is compatible: live connections must survive it.
        let widened = BASE.replace("max-per-endpoint=8", "max-per-endpoint=64");
        let (after, resolutions) =
            UpstreamRuntimes::resolve(&plan(&widened), &state, std::path::Path::new(".")).unwrap();

        assert!(resolutions[0].1.all_reused(), "{:?}", resolutions[0].1);
        let before = &before.runtimes[0];
        let after = &after.runtimes[0];
        assert!(Arc::ptr_eq(&before.pool, &after.pool));
        assert!(Arc::ptr_eq(&before.selector, &after.selector));
        assert!(Arc::ptr_eq(&before.endpoints, &after.endpoints));
    }

    #[test]
    fn replacing_the_backends_replaces_endpoints_and_pool_but_not_the_cursor() {
        let state = StateRegistry::new();
        let (before, _) = UpstreamRuntimes::resolve(&plan(BASE), &state, std::path::Path::new(".")).unwrap();
        let moved = BASE.replace("127.0.0.1:9000", "127.0.0.1:9001");
        let (after, resolutions) =
            UpstreamRuntimes::resolve(&plan(&moved), &state, std::path::Path::new(".")).unwrap();

        let resolution = resolutions[0].1;
        assert_eq!(resolution.endpoints, Resolution::Replaced);
        assert_eq!(resolution.pool, Resolution::Replaced);
        // The selector's compatibility includes the endpoint set, so it is
        // replaced too: a cursor into a set that no longer exists is meaningless.
        assert_eq!(resolution.selector, Resolution::Replaced);
        assert!(!Arc::ptr_eq(&before.runtimes[0].pool, &after.runtimes[0].pool));
    }

    #[test]
    fn changing_only_the_strategy_keeps_connections_and_health() {
        let state = StateRegistry::new();
        let (before, _) = UpstreamRuntimes::resolve(&plan(BASE), &state, std::path::Path::new(".")).unwrap();
        let rebalanced = BASE.replace(
            "pool max-per-endpoint=8",
            "balance \"least-conn\"\n    pool max-per-endpoint=8",
        );
        let (after, resolutions) =
            UpstreamRuntimes::resolve(&plan(&rebalanced), &state, std::path::Path::new(".")).unwrap();

        let resolution = resolutions[0].1;
        assert_eq!(resolution.selector, Resolution::Replaced);
        assert_eq!(resolution.pool, Resolution::Reused);
        assert_eq!(resolution.endpoints, Resolution::Reused);
        assert!(Arc::ptr_eq(&before.runtimes[0].pool, &after.runtimes[0].pool));
    }

    #[test]
    fn removing_an_upstream_retires_its_state() {
        let state = StateRegistry::new();
        UpstreamRuntimes::resolve(&plan(BASE), &state, std::path::Path::new(".")).unwrap();
        assert_eq!(state.len(), 3);

        let without = r#"
global { http ":8080" }
site "*" { route "/" { respond } }
"#;
        let plan = plan(without);
        UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();
        let retired = state.retain(&plan.bindings);
        assert_eq!(retired.len(), 3);
        assert!(state.is_empty());
    }

    #[test]
    fn upstreams_are_addressable_by_identity() {
        let state = StateRegistry::new();
        let plan = plan(BASE);
        let (runtimes, _) = UpstreamRuntimes::resolve(&plan, &state, std::path::Path::new(".")).unwrap();
        let id = gatel_model::ResourceId::new(gatel_model::ResourceKind::Upstream, "api");
        assert!(runtimes.get(&id).is_some());
        assert!(
            runtimes
                .get(&gatel_model::ResourceId::new(
                    gatel_model::ResourceKind::Upstream,
                    "absent"
                ))
                .is_none()
        );
    }
}
