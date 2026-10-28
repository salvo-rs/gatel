//! Wiring: the pieces that every subcommand needs, assembled in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gatel_http::{BindTable, Resources, Runtime, Server, prepare};
use gatel_model::{Diagnostics, Snapshot, SourceId, SourceSnapshot};
use gatel_observe::{Live, Telemetry};
use gatel_plan::{GenerationStore, Outcome, Registry};
use gatel_state::{StateRegistry, Supervisor};
use gatel_tls::{CertificateStore, InternalCa};

#[path = "app_submissions.rs"]
mod submissions;

#[path = "app_health.rs"]
mod health;

#[path = "app_recovery.rs"]
mod recovery;

#[path = "app_completion.rs"]
mod completion;

#[cfg(feature = "admin-api")]
#[path = "app_checkpoint.rs"]
mod checkpoint;
#[cfg(feature = "admin-api")]
pub(crate) use checkpoint::session::CheckpointSession;

#[path = "app_writer.rs"]
mod writer;
#[cfg(feature = "admin-api")]
pub(crate) use writer::AppSourceWriter;

/// Includes queued work and activation, not just the blocking worker.
const MAX_RELOAD_TASKS: usize = 8;

#[derive(Default)]
struct Lifecycle {
    closed: bool,
    acme: Option<Arc<gatel_tls::Acme>>,
}

/// Everything the process owns across generations.
pub struct App {
    /// Serializes complete activation with terminal shutdown, before reading
    /// the generation that activation is about to apply.
    lifecycle: tokio::sync::Mutex<Lifecycle>,
    /// Only advanced after complete activation; publication alone is not enough.
    activated: std::sync::atomic::AtomicU64,
    submissions: Arc<submissions::Submissions>,
    configuration_slot: Arc<tokio::sync::Semaphore>,
    /// One owned high-level reload from observation through activation. Future
    /// checkpoint work belongs inside this gate, not after returning Outcome.
    reload_execution: tokio::sync::Mutex<()>,
    reload_slots: Arc<tokio::sync::Semaphore>,
    blocking_budget: Arc<gatel_plan::blocking::Budget>,
    pub registry: Registry,
    // Only the host transaction module and its children can publish directly.
    // Transports/watchers receive status copies, never the writable store.
    store: Arc<GenerationStore<Arc<Runtime>>>,
    pub binds: Arc<BindTable>,
    pub state: Arc<StateRegistry>,
    pub server: Arc<Server>,
    /// Background workers. Reconciled after a commit, never before: a
    /// candidate that fails must not have started anything.
    pub supervisor: Arc<Supervisor>,
    /// Certificates outlive generations: a renewal is not a configuration
    /// change and must not produce one.
    pub certificates: Arc<CertificateStore>,
    /// ACME challenges outstanding right now. Process-wide and outside every
    /// generation: an order started under one configuration must still be
    /// answerable after a reload.
    pub challenges: Arc<gatel_tls::Challenges>,
    /// Most recent preparation cache. Running-task ownership is separate in
    /// lifecycle; a failed candidate must not choose what shutdown stops.
    pub acme: std::sync::Mutex<Option<(gatel_model::Fingerprint, Arc<gatel_tls::Acme>)>>,
    /// What the process measures and reports about itself. Older than any
    /// generation, so a counter reads across a reload.
    pub telemetry: Arc<Telemetry>,
    pub root: PathBuf,
    source: SourceId,
    /// The last snapshot, so an unchanged file keeps its revision.
    previous: std::sync::Mutex<Option<SourceSnapshot>>,
    /// CLI-generated immutable input. In this mode root is a directory.
    fixed: Option<Snapshot>,
    files: Option<crate::file_sources::FileSources>,
}

impl App {
    /// Use the same compile/prepare/commit lifecycle without a file frontend.
    pub fn from_snapshot(directory: PathBuf, snapshot: Snapshot) -> Result<Self, String> {
        let mut app = Self::with_sources(directory, snapshot.sources().map(|source| source.source.clone()))?;
        app.fixed = Some(snapshot);
        Ok(app)
    }

    pub fn watches_configuration(&self) -> bool {
        self.fixed.is_none()
    }

    pub fn new(root: PathBuf) -> Result<Self, String> {
        Self::with_sources(root, [SourceId::file("main")])
    }

    pub fn new_with_files(
        root: PathBuf,
        sources: Vec<crate::file_sources::NamedFile>,
    ) -> Result<Self, String> {
        if sources.is_empty() {
            return Self::new(root);
        }
        let files = crate::file_sources::FileSources::new(&root, sources)?;
        let mut app = Self::with_sources(root, files.identities())?;
        app.files = Some(files);
        Ok(app)
    }

    #[cfg(feature = "admin-api")]
    pub fn new_with_admin(
        root: PathBuf,
        sources: Vec<crate::file_sources::NamedFile>,
    ) -> Result<Self, String> {
        let files = crate::file_sources::FileSources::new(&root, sources)?;
        let mut app = Self::with_sources(root, files.identities().chain([SourceId::admin()]))?;
        app.files = Some(files);
        Ok(app)
    }

    fn with_sources(root: PathBuf, sources: impl IntoIterator<Item = SourceId>) -> Result<Self, String> {
        let telemetry = Telemetry::new();
        let registry = crate::plugins::registry()?;
        let store = Arc::new(GenerationStore::with_allowed_sources(sources));
        assert!(
            telemetry
                .health
                .set_reload_status(Arc::new(health::ConfigurationStatus(Arc::downgrade(&store))))
        );
        let binds = Arc::new(BindTable::new());
        let blocking_budget = Arc::new(gatel_plan::blocking::Budget::new(Default::default())?);
        let server = Arc::new(Server::with_blocking_executor(
            Arc::clone(&store),
            Arc::clone(&binds),
            blocking_budget.executor(),
        ));
        Ok(Self {
            lifecycle: tokio::sync::Mutex::new(Lifecycle::default()),
            activated: std::sync::atomic::AtomicU64::new(0),
            submissions: Arc::default(),
            configuration_slot: Arc::new(tokio::sync::Semaphore::new(1)),
            reload_execution: tokio::sync::Mutex::new(()),
            reload_slots: Arc::new(tokio::sync::Semaphore::new(MAX_RELOAD_TASKS)),
            blocking_budget,
            registry,
            store,
            binds,
            state: Arc::new(StateRegistry::new()),
            server,
            supervisor: Arc::new(Supervisor::new(Arc::clone(&telemetry))),
            certificates: Arc::new(CertificateStore::new()),
            challenges: gatel_tls::Challenges::new(),
            acme: std::sync::Mutex::new(None),
            telemetry,
            root,
            source: SourceId::file("main"),
            previous: std::sync::Mutex::new(None),
            fixed: None,
            files: None,
        })
    }

    /// Read the configuration tree into a snapshot of every source.
    pub fn observe(&self) -> Result<Snapshot, Diagnostics> {
        self.observe_from(&self.store.observed_snapshot().1)
    }

    fn observe_from(&self, admitted: &Snapshot) -> Result<Snapshot, Diagnostics> {
        if let Some(snapshot) = &self.fixed {
            return Ok(snapshot.clone());
        }
        if let Some(files) = &self.files {
            return files.observe_from(admitted);
        }
        self.observe_source_from(admitted.get(&self.source), |previous| {
            gatel_kdl::snapshot(&self.root, self.source.clone(), previous)
        })
    }

    #[cfg(test)]
    fn observe_source(
        &self,
        read: impl FnOnce(Option<&SourceSnapshot>) -> Result<SourceSnapshot, Diagnostics>,
    ) -> Result<Snapshot, Diagnostics> {
        self.observe_source_from(None, read)
    }

    fn observe_source_from(
        &self,
        admitted: Option<&SourceSnapshot>,
        read: impl FnOnce(Option<&SourceSnapshot>) -> Result<SourceSnapshot, Diagnostics>,
    ) -> Result<Snapshot, Diagnostics> {
        // Serialize observation and revision assignment, not preparation or
        // serving. Two reads must not derive different content from the same
        // previous revision and then publish the cache in reverse order.
        let mut previous = self.previous.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = read(crate::file_sources::newest(previous.as_ref(), admitted))?;
        *previous = Some(snapshot.clone());
        Ok(Snapshot::new().with(snapshot))
    }

    /// Run one configuration transaction end to end.
    pub async fn reload(self: &Arc<Self>) -> Outcome {
        let result = if let Some(snapshot) = &self.fixed {
            self.reload_snapshot_if(self.store.status().version(), snapshot.clone())
                .await
        } else {
            self.reload_task(|app| async move { Ok(app.attempt_async().await) })
                .await
        };
        result.unwrap_or_else(|_| {
            let outcome = Outcome::Stale("configuration changed before immutable input submission".into());
            outcome.record(&self.telemetry);
            outcome
        })
    }

    /// Submit an authorized complete union with bounded preparation and owned
    /// activation. The version is checked when execution begins, not merely
    /// when queued. This does not persist intent or replace frontend inputs.
    pub async fn reload_snapshot_if(
        self: &Arc<Self>,
        expected: gatel_plan::Version,
        snapshot: Snapshot,
    ) -> Result<Outcome, gatel_plan::Conflict> {
        self.reload_task(move |app| async move {
            app.configuration_work(move |app| app.attempt_snapshot_if(&expected, &snapshot))
                .await
                .unwrap_or_else(|| {
                    let outcome = Outcome::Stale("configuration submissions are closed".into());
                    outcome.record(&app.telemetry);
                    Ok(outcome)
                })
        })
        .await
    }

    async fn reload_task<F, Fut, E>(self: &Arc<Self>, work: F) -> Result<Outcome, E>
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Outcome, E>> + Send,
        E: Send + 'static,
    {
        self.reload_task_finishing(work, |_, result| async move { result })
            .await
    }

    /// Host-only receipt boundary. Completion remains owned and serialized after
    /// activation, including when the request waiter disappears. It also sees
    /// work errors so a journal adapter can classify a previously written intent.
    /// Admission rejection runs neither callback. This hook alone promises no
    /// persistence: adapters must perform bounded I/O and map uncertain outcomes.
    async fn reload_task_finishing<F, Fut, E, Finish, Finished>(
        self: &Arc<Self>,
        work: F,
        finish: Finish,
    ) -> Result<Outcome, E>
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Outcome, E>> + Send,
        E: Send + 'static,
        Finish: FnOnce(Arc<Self>, Result<Outcome, E>) -> Finished + Send + 'static,
        Finished: std::future::Future<Output = Result<Outcome, E>> + Send,
    {
        let slot = match self.reload_slots.clone().try_acquire_owned() {
            Ok(slot) => slot,
            Err(error) => {
                let reason = match error {
                    tokio::sync::TryAcquireError::Closed => "configuration submissions are closed",
                    tokio::sync::TryAcquireError::NoPermits => "configuration reload capacity exhausted",
                };
                let outcome = Outcome::Stale(reason.into());
                outcome.record(&self.telemetry);
                return Ok(outcome);
            }
        };
        let Some(admission) = self.submissions.enter() else {
            let outcome = Outcome::Stale("configuration submissions are closed".into());
            outcome.record(&self.telemetry);
            return Ok(outcome);
        };
        let app = self.clone();
        let task = tokio::spawn(async move {
            let _slot = slot;
            let _admission = admission;
            // Keep ordering ownership in the spawned job, not its caller.
            // The blocking worker slot alone ends before activation and cannot
            // serialize a publication with its later persistence/receipt work.
            let _execution = app.reload_execution.lock().await;
            if app.store.is_sealed() {
                let outcome = Outcome::Stale("configuration submissions are closed".into());
                outcome.record(&app.telemetry);
                return Ok(outcome);
            }
            let outcome = work(app.clone()).await;
            // Health recovery reads the transaction store directly. Only an
            // as-yet unactivated NoOp needs lifecycle reconciliation; a lower
            // level caller may have published without activating it.
            let needs_activation = match &outcome {
                Ok(Outcome::Started { .. } | Outcome::Reloaded { .. }) => true,
                Ok(Outcome::NoOp { generation }) => {
                    app.activated.load(std::sync::atomic::Ordering::Acquire) < generation.number()
                }
                _ => false,
            };
            if needs_activation {
                app.activate().await;
            }
            finish(app.clone(), outcome).await
        });
        match task.await {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => panic!("configuration activation task cancelled unexpectedly"),
        }
    }

    /// Internal prepare/publish stage; reload_task owns activation and ordering.
    async fn attempt_async(self: &Arc<Self>) -> Outcome {
        self.configuration_work(Self::attempt).await.unwrap_or_else(|| {
            let outcome = Outcome::Stale("configuration submissions are closed".into());
            outcome.record(&self.telemetry);
            outcome
        })
    }

    pub(crate) async fn configuration_work<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&App) -> T + Send + 'static,
    ) -> Option<T> {
        let Ok(slot) = self.configuration_slot.clone().acquire_owned().await else {
            return None;
        };
        let admission = self.submissions.enter()?;
        let app = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            // Keep both guards in the worker, not the awaiting request. Dropping
            // its JoinHandle must neither free capacity nor hide work from drain.
            let _slot = slot;
            let _admission = admission;
            work(&app)
        });
        match task.await {
            Ok(outcome) => Some(outcome),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => panic!("configuration worker cancelled unexpectedly"),
        }
    }

    /// Internal synchronous stage. Production callers enter through reload_task.
    fn attempt(&self) -> Outcome {
        let (version, mut snapshot) = self.store.observed_snapshot();
        let observed = match self.observe_from(&snapshot) {
            Ok(snapshot) => snapshot,
            Err(diagnostics) => {
                let outcome = self
                    .store
                    .reject_observation_if(&version, diagnostics)
                    .unwrap_or_else(|_| {
                        Outcome::Stale("configuration changed during failed source observation".into())
                    });
                outcome.record(&self.telemetry);
                return outcome;
            }
        };
        // A file frontend owns only its registered file contributions. Keep
        // other sources' latest intent, including rejected input/tombstones.
        // Capture before I/O; conditional submission rejects concurrent updates
        // instead of borrowing their version for an obsolete composition.
        for source in observed.sources() {
            snapshot.insert(source.clone());
        }
        self.attempt_snapshot_if(&version, &snapshot).unwrap_or_else(|_| {
            let outcome =
                Outcome::Stale("configuration changed during source or resource observation".into());
            outcome.record(&self.telemetry);
            outcome
        })
    }

    /// Submit a complete, already-authorized source union through the normal
    /// runtime preparation/publication path. The caller owns source observation
    /// and persistence; this does not replace the file cache or immutable input.
    /// Successful publication still requires activate() to reconcile workers.
    fn attempt_snapshot_if(
        &self,
        expected: &gatel_plan::Version,
        snapshot: &Snapshot,
    ) -> Result<Outcome, gatel_plan::Conflict> {
        let Some(_admission) = self.submissions.enter() else {
            let outcome = Outcome::Stale("configuration submissions are closed".into());
            outcome.record(&self.telemetry);
            return Ok(outcome);
        };
        if self.store.is_sealed() {
            let outcome = Outcome::Stale("configuration publication is sealed".into());
            outcome.record(&self.telemetry);
            return Ok(outcome);
        }
        let actual = self.store.status().version();
        if &actual != expected {
            return Err(gatel_plan::Conflict { actual });
        }
        // Observe resources under a captured generation version. If a commit
        // overtakes file reads, the conditional transaction refuses this
        // observation instead of attaching it to a different generation.
        let refresh = self.store.current().is_some_and(|current| {
            current
                .runtime
                .upstreams
                .tls_material_changed(self.root_directory())
                || current.runtime.server_tls.changed()
                || current.runtime.secrets.changed(self.root_directory())
        });
        let build =
            |preparation: gatel_plan::Preparation<'_, Arc<Runtime>>| self.prepare_runtime(preparation);
        let outcome = if refresh {
            self.store.refresh_if(expected, snapshot, &self.registry, build)
        } else {
            self.store.attempt_if(expected, snapshot, &self.registry, build)
        }?;
        outcome.record(&self.telemetry);
        Ok(outcome)
    }

    /// Open the account with the certificate authority, once.
    ///
    /// Memoised on what the configuration said, so a reload that leaves the
    /// authority alone keeps the account, its key and its rate-limit standing,
    /// and one that changes it opens a new one.
    fn acme_factory(&self) -> impl Fn(&gatel_plan::Acme) -> Result<Arc<gatel_tls::Acme>, String> + use<'_> {
        let root = self.root_directory().to_path_buf();
        move |config| {
            let mut hasher = gatel_model::Hasher::new();
            config.hash_into(&mut hasher);
            let fingerprint = hasher.finish();

            // A failed candidate may replace the preparation cache, but must
            // not make a retry forget the account owned by the committed runtime.
            if let Some(committed) = self.store.current()
                && let (Some(active_config), Some((active, _))) =
                    (&committed.plan.spec.global.acme, &committed.runtime.acme)
            {
                let mut hash = gatel_model::Hasher::new();
                active_config.hash_into(&mut hash);
                if hash.finish() == fingerprint {
                    return Ok(active.clone());
                }
            }

            let mut held = self.acme.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((known, acme)) = held.as_ref()
                && *known == fingerprint
            {
                return Ok(Arc::clone(acme));
            }
            let acme = gatel_tls::Acme::new(
                config,
                &root,
                Arc::clone(&self.challenges),
                Arc::clone(&self.certificates),
                Arc::clone(&self.telemetry),
            )?;
            *held = Some((fingerprint, Arc::clone(&acme)));
            Ok(acme)
        }
    }

    /// Compile without preparing anything, for `validate` and `explain`.
    pub fn compile(&self) -> Result<Arc<gatel_plan::Plan>, Diagnostics> {
        let snapshot = self.observe()?;
        let spec = gatel_plan::normalize(&snapshot, &self.registry)?;
        Ok(Arc::new(gatel_plan::compile(Arc::new(spec), &self.registry)))
    }

    /// Bring background workers and accept loops in line with the generation
    /// that was just committed, and release the state nothing needs any more.
    async fn activate(&self) {
        let mut lifecycle = self.lifecycle.lock().await;
        if lifecycle.closed {
            return;
        }
        loop {
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
            // After the commit, never before: a candidate that failed to prepare
            // must not have asked a certificate authority for anything.
            match &committed.runtime.acme {
                Some((acme, wanted)) => {
                    if lifecycle
                        .acme
                        .as_ref()
                        .is_some_and(|active| !Arc::ptr_eq(active, acme))
                        && let Some(previous) = lifecycle.acme.as_ref()
                    {
                        previous.shutdown().await;
                    }
                    lifecycle.acme = Some(acme.clone());
                    acme.reconcile(wanted.clone()).await;
                }
                // Keep ownership across awaits so cancellation can retry the
                // old account's drain. This is the async lifecycle mutex.
                None => {
                    if let Some(acme) = lifecycle.acme.as_ref() {
                        acme.reconcile(Vec::new()).await;
                    }
                    lifecycle.acme = None;
                }
            }
            // A commit may land while accept loops or ACME work were awaited.
            // Catch up while still holding activation ownership, before pruning
            // state and metrics using an obsolete generation.
            if !self
                .store
                .current()
                .is_some_and(|current| Arc::ptr_eq(&current, &committed))
            {
                continue;
            }
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
            self.activated
                .store(committed.number.number(), std::sync::atomic::Ordering::Release);
            return;
        }
    }

    /// Stop accepting, stop probing, and let what is in flight finish.
    pub async fn shutdown(&self) {
        self.reload_slots.close();
        self.configuration_slot.close();
        self.submissions.close();
        // Fence publication before waiting for any activation or resource drain.
        // An in-flight preparation can finish, but cannot commit after this.
        self.store.seal();
        {
            let mut lifecycle = self.lifecycle.lock().await;
            lifecycle.closed = true;
            // Before the accept loops stop, so a load balancer reading readiness
            // has the grace period to take this instance out.
            self.telemetry.health.draining();
        }
        // An admitted reload may be queued in activate(). Let it acquire the
        // lifecycle lock, observe closed, and release its admission before drain.
        self.submissions.drained().await;
        let mut lifecycle = self.lifecycle.lock().await;
        self.server.shutdown().await;
        // Retaining App for inspection must not retain listening sockets.
        // All admitted preparation and accept loops have finished by now.
        self.binds.retain(&[]);
        self.supervisor.shutdown().await;
        if let Some(committed) = self.store.current() {
            committed.runtime.plugin_tasks.shutdown().await;
        }
        if let Some(acme) = lifecycle.acme.as_ref() {
            acme.shutdown().await;
        }
        lifecycle.acme = None;
        drop(lifecycle);
        // Logging gets a separate, explicit budget after request/plugin drain.
        // Closing queues works even while retained generations still own Sink
        // handles. Cancelling the wait cannot interrupt a blocked OS write.
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.telemetry.sinks.shutdown())
            .await
            .is_err()
        {
            self.telemetry.sinks.record_shutdown_timeout();
        }
    }

    /// Every file the configuration tree touched, so a watcher knows what to
    /// watch — including files an `import` pulled in.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        if self.fixed.is_some() {
            return Vec::new();
        }
        if let Some(files) = &self.files {
            return files.watched_paths();
        }
        match gatel_kdl::load(&self.root) {
            Ok(loaded) => loaded.files,
            // A configuration that will not load still has a root file to
            // watch, which is how a broken file can be fixed in place.
            Err(_) => vec![self.root.clone()],
        }
    }

    pub fn root_directory(&self) -> &Path {
        if self.fixed.is_some() {
            return &self.root;
        }
        self.root.parent().unwrap_or(Path::new("."))
    }
}

#[cfg(test)]
#[path = "app_observation_tests.rs"]
mod observation_tests;

#[cfg(test)]
#[path = "app_lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "app_log_shutdown_tests.rs"]
mod log_shutdown_tests;

#[cfg(test)]
#[path = "app_server_tls_tests.rs"]
mod server_tls_tests;

#[cfg(test)]
#[path = "app_submission_tests.rs"]
mod submission_tests;

#[cfg(test)]
#[path = "app_reload_execution_tests.rs"]
mod reload_execution_tests;

#[cfg(all(test, feature = "admin-api"))]
#[path = "app_status_progress_tests.rs"]
mod status_progress_tests;

#[cfg(test)]
#[path = "app_source_union_tests.rs"]
mod source_union_tests;

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
