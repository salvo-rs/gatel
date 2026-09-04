//! Turning a plan into something that can serve.
//!
//! Everything that can fail for a reason configuration cannot predict happens
//! here: binding sockets, building handlers, resolving state. It runs before
//! anything is published, so a failure leaves the running generation exactly as
//! it was.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gatel_plan::{Limits, Plan, Preparation, Terminal};
use gatel_state::{StateRegistry, UpstreamRuntimes};
use gatel_tls::{CertificateStore, InternalCa, ServerConfig, TlsPreparation};
use salvo_core::{Router, Service};

use crate::bind::BindTable;
use crate::dispatch::{Dispatcher, PreparedRoute, PreparedSite};

/// What preparation is allowed to touch: everything that outlives a generation.
pub struct Resources<'a> {
    pub binds: &'a BindTable,
    pub state: &'a StateRegistry,
    pub certificates: &'a Arc<CertificateStore>,
    /// What the process measures. Older than any generation, which is why a
    /// counter reads across a reload rather than restarting at zero.
    pub telemetry: &'a Arc<gatel_observe::Telemetry>,
    /// The directory `file:` references resolve against.
    pub root: &'a Path,
    /// Where the development authority lives, if a site asks for one.
    pub internal_ca: PathBuf,
}

/// One listener, ready to serve.
pub struct ListenerRuntime {
    pub name: String,
    pub bind: SocketAddr,
    /// Present exactly when this listener terminates TLS. Immutable for the
    /// life of the generation; the certificates it resolves are not.
    pub tls: Option<Arc<ServerConfig>>,
    pub service: Arc<Service>,
    /// Transport counters. Shared with every generation that keeps this
    /// listener, so accepting a connection does not restart the count.
    pub metrics: Arc<gatel_observe::metrics::Listener>,
}

impl ListenerRuntime {
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }
}

impl std::fmt::Debug for ListenerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerRuntime")
            .field("name", &self.name)
            .field("bind", &self.bind)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

/// Everything one generation needs to serve traffic.
#[derive(Debug)]
pub struct Runtime {
    pub listeners: BTreeMap<SocketAddr, Arc<ListenerRuntime>>,
    pub upstreams: Arc<UpstreamRuntimes>,
    pub limits: Limits,
    pub grace_period: Duration,
}

impl Runtime {
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.listeners.keys().copied().collect()
    }
}

/// Build a runtime for a candidate plan.
pub fn prepare(
    preparation: &Preparation<'_, Arc<Runtime>>,
    resources: &Resources<'_>,
) -> Result<Arc<Runtime>, String> {
    let plan: &Arc<Plan> = preparation.plan;

    // Bind first: a port conflict is the most common reason a reload cannot
    // proceed, and finding it before building handlers keeps the failure cheap.
    let wanted: Vec<SocketAddr> = plan
        .listeners
        .iter()
        .map(|listener| listener.listener.bind)
        .collect();
    resources.binds.ensure(&wanted)?;

    let (upstreams, resolutions) = UpstreamRuntimes::resolve(plan, resources.state);
    let upstreams = Arc::new(upstreams);
    for (id, resolution) in &resolutions {
        if !resolution.all_reused() {
            tracing::debug!(
                target: "gatel::runtime",
                event = "upstream_state_resolved",
                generation = preparation.generation.number(),
                upstream = id.name(),
                endpoints = ?resolution.endpoints,
                selector = ?resolution.selector,
                pool = ?resolution.pool,
            );
        }
    }

    let internal_ca_directory = resources.internal_ca.clone();
    let internal = move || InternalCa::load_or_create(&internal_ca_directory).map(Arc::new);

    let mut listeners = BTreeMap::new();
    for listener_plan in &plan.listeners {
        let listener_metrics = resources.telemetry.metrics.listener(&listener_plan.listener.name);
        // Certificates load here, so a bad one stops the reload rather than
        // breaking the first handshake after it.
        let tls = gatel_tls::prepare_listener(
            listener_plan,
            &TlsPreparation {
                store: resources.certificates,
                internal: &internal,
                root: resources.root,
            },
        )?;

        let mut sites = Vec::with_capacity(listener_plan.sites.len());
        for site in &listener_plan.sites {
            let mut routes = Vec::with_capacity(site.routes.len());
            for route in &site.routes {
                let mut chain = Vec::with_capacity(route.chain.len() + 1);
                for directive in &route.chain {
                    let state = resolve_state(directive, resources.state);
                    let cx = gatel_plan::BuildContext {
                        root: resources.root,
                        state,
                        telemetry: resources.telemetry,
                    };
                    chain.push(directive.config.build(cx).map_err(|error| {
                        format!(
                            "`{}` in route `{}`: {error}",
                            directive.meta.name,
                            route.id.name()
                        )
                    })?);
                }
                chain.push(build_terminal(
                    &route.terminal,
                    &upstreams,
                    resources,
                    route.id.name(),
                )?);
                routes.push(PreparedRoute {
                    id: route.id.clone(),
                    path: route.path.clone(),
                    methods: route.methods.clone(),
                    matchers: route.matchers.clone(),
                    chain,
                    terminal_name: terminal_name(&route.terminal),
                    metrics: resources.telemetry.metrics.route(site.id.name(), route.id.name()),
                });
            }
            sites.push(PreparedSite {
                id: site.id.clone(),
                host: site.host.clone(),
                routes,
            });
        }

        let dispatcher = Dispatcher {
            sites,
            tls: listener_plan.listener.protocol.is_tls(),
            trusted_proxies: plan.spec.global.trusted_proxies.clone(),
            metrics: Arc::clone(&listener_metrics),
        };

        listeners.insert(
            listener_plan.listener.bind,
            Arc::new(ListenerRuntime {
                name: listener_plan.listener.name.clone(),
                bind: listener_plan.listener.bind,
                tls,
                service: Arc::new(Service::new(root_router(dispatcher))),
                metrics: listener_metrics,
            }),
        );
    }

    Ok(Arc::new(Runtime {
        listeners,
        upstreams,
        limits: plan.spec.global.limits.clone(),
        grace_period: plan.spec.global.grace_period,
    }))
}

/// Gatel routes, so Salvo's router is a single node whose goal is the
/// dispatcher. The filter consumes the whole path because Salvo only selects a
/// goal once the path is fully matched, and here the path is matched by us.
fn root_router(dispatcher: Dispatcher) -> Router {
    Router::new()
        .filter_fn(|_request, path_state| {
            path_state.forward(usize::MAX >> 2);
            true
        })
        .goal(dispatcher)
}

fn terminal_name(terminal: &Terminal) -> &'static str {
    match terminal {
        Terminal::Proxy { .. } => "proxy",
        Terminal::Directive(instance) => instance.meta.name,
    }
}

/// Resolve whatever a directive said it needs to keep between requests.
///
/// Reusing is the normal case: a reload that did not change a rate limit must
/// not hand every client a fresh bucket.
fn resolve_state(
    directive: &gatel_plan::DirectiveInstance,
    state: &StateRegistry,
) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
    let binding = directive.state_binding()?;
    let declared = directive.config.state()?;
    let (value, _) = state.bind_any(&binding, &declared.create);
    Some(value)
}

fn build_terminal(
    terminal: &Terminal,
    upstreams: &UpstreamRuntimes,
    resources: &Resources<'_>,
    route: &str,
) -> Result<Arc<dyn salvo_core::Handler>, String> {
    match terminal {
        Terminal::Proxy { upstream } => {
            let runtime = upstreams.get(upstream).ok_or_else(|| {
                format!("route `{route}` refers to upstream `{upstream}`, which has no runtime")
            })?;
            Ok(Arc::new(crate::proxy::ProxyTerminal::new(
                Arc::clone(runtime),
                resources.telemetry.metrics.upstream(upstream.name()),
            )))
        }
        Terminal::Directive(instance) => {
            let state = resolve_state(instance, resources.state);
            let cx = gatel_plan::BuildContext {
                root: resources.root,
                state,
                telemetry: resources.telemetry,
            };
            instance
                .config
                .build(cx)
                .map_err(|error| format!("`{}` in route `{route}`: {error}", instance.meta.name))
        }
    }
}
