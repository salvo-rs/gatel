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

/// Who obtains certificates, and for which sites.
pub type AcmeIntent = (Arc<gatel_tls::Acme>, Vec<gatel_tls::Wanted>);

/// What preparation is allowed to touch: everything that outlives a generation.
#[derive(Clone)]
pub struct Resources<'a> {
    /// Optional host controller. Without it explicit KDL budget changes reject.
    pub blocking_budget: Option<&'a gatel_plan::blocking::Budget>,
    pub blocking: &'a gatel_plan::blocking::Executor,
    pub plugin_retirements: &'a Arc<crate::plugin_retirement::Retirements>,
    pub binds: &'a BindTable,
    pub state: &'a StateRegistry,
    pub certificates: &'a Arc<CertificateStore>,
    /// ACME challenges outstanding for the process, answered by every listener.
    pub challenges: &'a Arc<gatel_tls::Challenges>,
    /// Opens the account with the certificate authority, once for the process.
    pub acme: &'a dyn Fn(&gatel_plan::Acme) -> Result<Arc<gatel_tls::Acme>, String>,
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
    /// Every connection here begins with a PROXY header, and only a trusted
    /// peer may open one.
    pub proxy_protocol: bool,
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
    sink_change: gatel_observe::sink::SinkUpdate,
    bind_change: crate::bind::BindUpdate,
    certificate_change: gatel_tls::store::CertificateUpdates,
    state_change: gatel_state::registry::StateUpdate,
    pub secrets: Arc<gatel_plan::SecretDependencies>,
    /// Exact manual server identity and inbound client-CA observations.
    pub server_tls: Arc<gatel_tls::ServerMaterial>,
    blocking_change: Option<gatel_plan::blocking::PreparedChange>,
    pub plugin_tasks: crate::plugin_tasks::PluginTasks,
    pub listeners: BTreeMap<SocketAddr, Arc<ListenerRuntime>>,
    pub upstreams: Arc<UpstreamRuntimes>,
    /// The sites that want automatic certificates, and who gets them. Empty
    /// unless the configuration asked. Reconciled after the commit, like a
    /// health check, so a candidate that fails starts no orders.
    pub acme: Option<AcmeIntent>,
    pub limits: Limits,
    pub grace_period: Duration,
    pub reload_drain: Option<Duration>,
    /// Who may speak for somebody else. Read when a PROXY header arrives, by
    /// the same rule that decides whether a forwarding header is believed.
    pub trusted_proxies: Vec<(std::net::IpAddr, u8)>,
}

impl Runtime {
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.listeners.keys().copied().collect()
    }
}

pub struct RuntimePublication<'a> {
    sinks: gatel_observe::sink::SinkPublication<'a>,
    bindings: crate::bind::BindPublication<'a>,
    certificates: gatel_tls::store::CertificatePublication<'a>,
    state: gatel_state::registry::StatePublication<'a>,
    blocking: Option<gatel_plan::blocking::Publication<'a>>,
    upstreams: &'a UpstreamRuntimes,
}

impl gatel_plan::publication::PublishGuard for RuntimePublication<'_> {
    fn published(self) {
        // Keep the registry guard until pool policies have caught up. Another
        // candidate must not stage/publish newer state and then have an older
        // publication overwrite its pool policies.
        self.upstreams.adopt_policies();
        self.sinks.publish();
        self.bindings.publish();
        self.certificates.publish();
        self.state.publish();
        if let Some(blocking) = self.blocking {
            blocking.publish();
        }
    }
}

impl gatel_plan::publication::PublishRuntime for Runtime {
    type Guard<'a> = RuntimePublication<'a>;
    fn finish_publication(&self) {
        self.bind_change.finish();
        self.certificate_change.finish();
        self.state_change.finish();
    }
    fn begin_publication(&self) -> Self::Guard<'_> {
        RuntimePublication {
            sinks: self.sink_change.begin(),
            bindings: self.bind_change.begin(),
            state: self.state_change.begin(),
            certificates: self.certificate_change.begin(),
            blocking: self
                .blocking_change
                .as_ref()
                .map(|change| change.begin_publication()),
            upstreams: &self.upstreams,
        }
    }
}

/// Build a runtime for a candidate plan.
pub fn prepare(
    preparation: &Preparation<'_, Arc<Runtime>>,
    resources: &Resources<'_>,
) -> Result<Arc<Runtime>, String> {
    let state_owner = resources.state.clone();
    let staged_state = resources.state.stage();
    let staged_resources = Resources {
        state: &staged_state,
        ..resources.clone()
    };
    let resources = &staged_resources;
    let plan: &Arc<Plan> = preparation.plan;
    let mut certificate_change =
        gatel_tls::store::CertificateUpdates::new(Arc::clone(resources.certificates));
    let blocking_change = match resources.blocking_budget {
        Some(budget) => {
            Some(budget.prepare(resources.blocking, plan.spec.global.blocking.unwrap_or_default())?)
        }
        None if plan.spec.global.blocking.is_some() => {
            return Err("host does not provide a configurable blocking budget".into());
        }
        None => None,
    };

    // Bind first: a port conflict is the most common reason a reload cannot
    // proceed, and finding it before building handlers keeps the failure cheap.
    let wanted: Vec<SocketAddr> = plan
        .listeners
        .iter()
        .map(|listener| listener.listener.bind)
        .collect();
    let bind_change = resources.binds.stage(&wanted)?;

    let (upstreams, resolutions) = UpstreamRuntimes::prepare(plan, resources.state, resources.root)?;
    let upstreams = Arc::new(upstreams);
    // The same handle every directive that has to ask a backend something is
    // built with. It is a generation's worth of backends, so a directive holds
    // the set its own generation had rather than whatever is current later.
    let reachable: gatel_plan::SharedUpstreams = Arc::clone(&upstreams) as _;
    let sinks = resources.telemetry.sinks.stage();
    let plugins = crate::plugin_prepare::PreparedPlugins::prepare(plan, resources, &reachable, &sinks)?;
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

    let server_tls = Arc::new(gatel_tls::ServerMaterial::default());
    let mut listeners = BTreeMap::new();
    for listener_plan in &plan.listeners {
        let listener_metrics = resources.telemetry.metrics.listener(&listener_plan.listener.name);
        // Certificates load here, so a bad one stops the reload rather than
        // breaking the first handshake after it.
        let tls = gatel_tls::prepare_listener_observed(
            listener_plan,
            plan.spec.global.acme.as_ref(),
            &TlsPreparation {
                updates: Some(&certificate_change),
                store: resources.certificates,
                internal: &internal,
                acme: resources.acme,
                root: resources.root,
            },
            &server_tls,
        )?;

        let mut sites = Vec::with_capacity(listener_plan.sites.len());
        for site in &listener_plan.sites {
            let mut routes = Vec::with_capacity(site.routes.len());
            for route in &site.routes {
                let mut chain = Vec::with_capacity(route.chain.len() + 1);
                for directive in &route.chain {
                    chain.push(plugins.handler(directive)?);
                }
                chain.push(build_terminal(
                    &route.terminal,
                    &upstreams,
                    resources,
                    &plugins,
                    route.id.name(),
                )?);
                routes.push(PreparedRoute {
                    execution: crate::execution::identity(route, plan),
                    id: route.id.clone(),
                    path: route.path.clone(),
                    methods: route.methods.clone(),
                    matchers: route.matchers.clone(),
                    chain,
                    terminal_name: terminal_name(&route.terminal),
                    metrics: resources.telemetry.metrics.route(site.id.name(), route.id.name()),
                });
            }
            // The site's own directives, plus the not-found terminal, for a
            // request that reached this site and matched none of its routes.
            let mut fallback = Vec::with_capacity(site.fallback.len() + 1);
            for directive in &site.fallback {
                fallback.push(plugins.handler(directive)?);
            }
            fallback.push(Arc::new(crate::dispatch::NotFound));

            sites.push(PreparedSite {
                fallback_execution: gatel_plan::ExecutionIdentity(plan.plan_hash()),
                id: site.id.clone(),
                host: site.host.clone(),
                routes,
                fallback,
            });
        }

        let dispatcher = Dispatcher {
            sites,
            tls: listener_plan.listener.protocol.is_tls(),
            trusted_proxies: plan.spec.global.trusted_proxies.clone(),
            max_body: plan.spec.global.limits.max_body.bytes(),
            challenges: Arc::clone(resources.challenges),
            metrics: Arc::clone(&listener_metrics),
        };

        listeners.insert(
            listener_plan.listener.bind,
            Arc::new(ListenerRuntime {
                name: listener_plan.listener.name.clone(),
                bind: listener_plan.listener.bind,
                tls,
                proxy_protocol: listener_plan.listener.proxy_protocol,
                service: Arc::new(Service::new(root_router(dispatcher))),
                metrics: listener_metrics,
            }),
        );
    }

    staged_state.retain(&plan.bindings);
    certificate_change.retain_sites(
        plan.listeners
            .iter()
            .filter(|listener| listener.listener.protocol.is_tls())
            .flat_map(|listener| listener.sites.iter().map(|site| site.id.clone())),
    );
    Ok(Arc::new(Runtime {
        sink_change: sinks.finish(),
        bind_change,
        certificate_change,
        state_change: state_owner.update_from(&staged_state),
        blocking_change,
        secrets: Arc::clone(&plugins.secrets),
        server_tls,
        plugin_tasks: plugins.tasks(resources)?,
        listeners,
        upstreams,
        acme: acme_wanted(plan, resources)?,
        limits: plan.spec.global.limits.clone(),
        grace_period: plan.spec.global.grace_period,
        reload_drain: plan.spec.global.reload_drain,
        trusted_proxies: plan.spec.global.trusted_proxies.clone(),
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
pub(crate) fn resolve_state(
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
    upstreams: &Arc<UpstreamRuntimes>,
    resources: &Resources<'_>,
    plugins: &crate::plugin_prepare::PreparedPlugins,
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
        Terminal::Directive(instance) => plugins.handler(instance),
    }
}

/// Which sites want an automatic certificate, and the account that gets them.
///
/// Collected from the plan rather than discovered while loading certificates,
/// so a site served on several listeners is asked for once.
fn acme_wanted(plan: &Plan, resources: &Resources<'_>) -> Result<Option<AcmeIntent>, String> {
    let mut wanted: Vec<gatel_tls::Wanted> = Vec::new();
    for site in &plan.spec.sites {
        let Some(tls) = &site.tls else { continue };
        if tls.source != gatel_plan::CertificateSource::Acme {
            continue;
        }
        let gatel_plan::HostMatch::Exact(host) = &site.host else {
            continue;
        };
        if wanted.iter().any(|one| one.host == *host) {
            continue;
        }
        wanted.push(gatel_tls::Wanted {
            site: site.id.clone(),
            host: host.clone(),
        });
    }
    if wanted.is_empty() {
        return Ok(None);
    }
    let Some(config) = plan.spec.global.acme.as_ref() else {
        return Ok(None);
    };
    Ok(Some(((resources.acme)(config)?, wanted)))
}
