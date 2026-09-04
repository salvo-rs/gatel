//! Wiring: the pieces that every subcommand needs, assembled in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gatel_http::{BindTable, Resources, Runtime, Server, prepare};
use gatel_model::{Diagnostics, Snapshot, SourceId, SourceSnapshot};
use gatel_observe::{Live, Telemetry};
use gatel_plan::{GenerationStore, Outcome, Registry};
use gatel_state::{StateRegistry, Supervisor};
use gatel_tls::{CertificateStore, InternalCa};

/// Everything the process owns across generations.
pub struct App {
    pub registry: Registry,
    pub store: Arc<GenerationStore<Arc<Runtime>>>,
    pub binds: Arc<BindTable>,
    pub state: Arc<StateRegistry>,
    pub server: Arc<Server>,
    /// Background workers. Reconciled after a commit, never before: a
    /// candidate that fails must not have started anything.
    pub supervisor: Arc<Supervisor>,
    /// Certificates outlive generations: a renewal is not a configuration
    /// change and must not produce one.
    pub certificates: Arc<CertificateStore>,
    /// What the process measures and reports about itself. Older than any
    /// generation, so a counter reads across a reload.
    pub telemetry: Arc<Telemetry>,
    pub root: PathBuf,
    source: SourceId,
    /// The last snapshot, so an unchanged file keeps its revision.
    previous: std::sync::Mutex<Option<SourceSnapshot>>,
}

impl App {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let telemetry = Telemetry::new();
        let registry = Registry::builder().bundle(gatel_bundle_std::bundle()).build()?;
        let store = Arc::new(GenerationStore::new());
        let binds = Arc::new(BindTable::new());
        let server = Arc::new(Server::new(Arc::clone(&store), Arc::clone(&binds)));
        Ok(Self {
            registry,
            store,
            binds,
            state: Arc::new(StateRegistry::new()),
            server,
            supervisor: Arc::new(Supervisor::new(Arc::clone(&telemetry))),
            certificates: Arc::new(CertificateStore::new()),
            telemetry,
            root,
            source: SourceId::file("main"),
            previous: std::sync::Mutex::new(None),
        })
    }

    /// Read the configuration tree into a snapshot of every source.
    pub fn observe(&self) -> Result<Snapshot, Diagnostics> {
        let previous = self.previous.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let snapshot = gatel_kdl::snapshot(&self.root, self.source.clone(), previous.as_ref())?;
        *self.previous.lock().unwrap_or_else(|p| p.into_inner()) = Some(snapshot.clone());
        Ok(Snapshot::new().with(snapshot))
    }

    /// Run one configuration transaction end to end.
    pub fn attempt(&self) -> Outcome {
        let snapshot = match self.observe() {
            Ok(snapshot) => snapshot,
            Err(diagnostics) => {
                let outcome = Outcome::Rejected(diagnostics);
                outcome.record(&self.telemetry);
                return outcome;
            }
        };
        let binds = Arc::clone(&self.binds);
        let state = Arc::clone(&self.state);
        let certificates = Arc::clone(&self.certificates);
        let telemetry = Arc::clone(&self.telemetry);
        let root = self.root_directory().to_path_buf();
        let outcome = self.store.attempt(&snapshot, &self.registry, move |preparation| {
            prepare(
                &preparation,
                &Resources {
                    binds: &binds,
                    state: &state,
                    certificates: &certificates,
                    telemetry: &telemetry,
                    root: &root,
                    internal_ca: InternalCa::default_directory(),
                },
            )
        });
        outcome.record(&self.telemetry);
        outcome
    }

    /// Compile without preparing anything, for `validate` and `explain`.
    pub fn compile(&self) -> Result<Arc<gatel_plan::Plan>, Diagnostics> {
        let snapshot = self.observe()?;
        let spec = gatel_plan::normalize(&snapshot, &self.registry)?;
        Ok(Arc::new(gatel_plan::compile(Arc::new(spec), &self.registry)))
    }

    /// Bring background workers and accept loops in line with the generation
    /// that was just committed, and release the state nothing needs any more.
    pub async fn activate(&self) {
        let Some(committed) = self.store.current() else {
            return;
        };
        self.server.sync().await;
        self.supervisor.reconcile(&committed.runtime.upstreams).await;
        // Readiness counts what is actually accepting, not what was asked for:
        // a generation whose addresses all failed to bind is not serving.
        self.telemetry
            .health
            .committed(self.server.active_addresses().await.len());
        self.telemetry.metrics.retain(&live_series(&committed.plan));
        // Only after the new generation is live: anything still held by a
        // draining request keeps working, it is simply no longer findable.
        for retired in self.state.retain(&committed.plan.bindings) {
            tracing::debug!(target: "gatel::runtime", event = "state_retired", state = %retired);
        }
        let sites: Vec<gatel_model::ResourceId> = committed
            .plan
            .spec
            .sites
            .iter()
            .map(|site| site.id.clone())
            .collect();
        for forgotten in self.certificates.retain(&sites) {
            tracing::debug!(
                target: "gatel::certificate",
                event = "certificate_forgotten",
                site = forgotten.name(),
            );
        }
    }

    /// Stop accepting, stop probing, and let what is in flight finish.
    pub async fn shutdown(&self) {
        // Before the accept loops stop, so a load balancer reading readiness
        // has the grace period to take this instance out.
        self.telemetry.health.draining();
        self.server.shutdown().await;
        self.supervisor.shutdown().await;
    }

    /// Every file the configuration tree touched, so a watcher knows what to
    /// watch — including files an `import` pulled in.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        match gatel_kdl::load(&self.root) {
            Ok(loaded) => loaded.files,
            // A configuration that will not load still has a root file to
            // watch, which is how a broken file can be fixed in place.
            Err(_) => vec![self.root.clone()],
        }
    }

    pub fn root_directory(&self) -> &Path {
        self.root.parent().unwrap_or(Path::new("."))
    }
}

/// The metric series a plan still names. Anything else is forgotten after the
/// commit, so a deleted route stops being reported rather than sitting at a
/// value that will never change again.
fn live_series(plan: &gatel_plan::Plan) -> Live {
    let mut live = Live::default();
    for listener in &plan.listeners {
        live.listener(&listener.listener.name);
        for site in &listener.sites {
            for route in &site.routes {
                live.route(site.id.name(), route.id.name());
            }
        }
    }
    for upstream in &plan.spec.upstreams {
        live.upstream(upstream.id.name());
    }
    live
}
