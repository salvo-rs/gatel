//! Turning a plan into something that can serve.
//!
//! Everything that can fail for a reason configuration cannot predict happens
//! here: binding sockets, building handlers, resolving state. It runs before
//! anything is published, so a failure leaves the running generation exactly as
//! it was.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gatel_plan::{Limits, Plan, Preparation, Terminal};
use gatel_state::{StateRegistry, UpstreamRuntimes};
use salvo_core::{Router, Service};

use crate::bind::BindTable;
use crate::dispatch::{Dispatcher, PreparedRoute, PreparedSite};

/// One listener, ready to serve.
pub struct ListenerRuntime {
    pub name: String,
    pub bind: SocketAddr,
    pub tls: bool,
    pub service: Arc<Service>,
}

impl std::fmt::Debug for ListenerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerRuntime")
            .field("name", &self.name)
            .field("bind", &self.bind)
            .field("tls", &self.tls)
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
    binds: &BindTable,
    state: &StateRegistry,
) -> Result<Arc<Runtime>, String> {
    let plan: &Arc<Plan> = preparation.plan;

    // Bind first: a port conflict is the most common reason a reload cannot
    // proceed, and finding it before building handlers keeps the failure cheap.
    let wanted: Vec<SocketAddr> = plan
        .listeners
        .iter()
        .map(|listener| listener.listener.bind)
        .collect();
    binds.ensure(&wanted)?;

    let (upstreams, resolutions) = UpstreamRuntimes::resolve(plan, state);
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

    let mut listeners = BTreeMap::new();
    for listener_plan in &plan.listeners {
        let mut sites = Vec::with_capacity(listener_plan.sites.len());
        for site in &listener_plan.sites {
            let mut routes = Vec::with_capacity(site.routes.len());
            for route in &site.routes {
                let mut chain = Vec::with_capacity(route.chain.len() + 1);
                for directive in &route.chain {
                    chain.push(directive.config.build().map_err(|error| {
                        format!(
                            "`{}` in route `{}`: {error}",
                            directive.meta.name,
                            route.id.name()
                        )
                    })?);
                }
                chain.push(build_terminal(&route.terminal, &upstreams, route.id.name())?);
                routes.push(PreparedRoute {
                    id: route.id.clone(),
                    path: route.path.clone(),
                    methods: route.methods.clone(),
                    matchers: route.matchers.clone(),
                    chain,
                    terminal_name: terminal_name(&route.terminal),
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
        };

        listeners.insert(
            listener_plan.listener.bind,
            Arc::new(ListenerRuntime {
                name: listener_plan.listener.name.clone(),
                bind: listener_plan.listener.bind,
                tls: listener_plan.listener.protocol.is_tls(),
                service: Arc::new(Service::new(root_router(dispatcher))),
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

fn build_terminal(
    terminal: &Terminal,
    upstreams: &UpstreamRuntimes,
    route: &str,
) -> Result<Arc<dyn salvo_core::Handler>, String> {
    match terminal {
        Terminal::Proxy { upstream } => {
            let runtime = upstreams.get(upstream).ok_or_else(|| {
                format!("route `{route}` refers to upstream `{upstream}`, which has no runtime")
            })?;
            Ok(Arc::new(crate::proxy::ProxyTerminal::new(Arc::clone(runtime))))
        }
        Terminal::Directive(instance) => instance
            .config
            .build()
            .map_err(|error| format!("`{}` in route `{route}`: {error}", instance.meta.name)),
    }
}
