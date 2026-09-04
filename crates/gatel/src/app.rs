//! Wiring: the pieces that every subcommand needs, assembled in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gatel_http::{BindTable, Runtime, Server, prepare};
use gatel_model::{Diagnostics, Snapshot, SourceId, SourceSnapshot};
use gatel_plan::{GenerationStore, Outcome, Registry};
use gatel_state::StateRegistry;

/// Everything the process owns across generations.
pub struct App {
    pub registry: Registry,
    pub store: Arc<GenerationStore<Arc<Runtime>>>,
    pub binds: Arc<BindTable>,
    pub state: Arc<StateRegistry>,
    pub server: Arc<Server>,
    pub root: PathBuf,
    source: SourceId,
    /// The last snapshot, so an unchanged file keeps its revision.
    previous: std::sync::Mutex<Option<SourceSnapshot>>,
}

impl App {
    pub fn new(root: PathBuf) -> Result<Self, String> {
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
            Err(diagnostics) => return Outcome::Rejected(diagnostics),
        };
        let binds = Arc::clone(&self.binds);
        let state = Arc::clone(&self.state);
        self.store.attempt(&snapshot, &self.registry, move |preparation| {
            prepare(&preparation, &binds, &state)
        })
    }

    /// Compile without preparing anything, for `validate` and `explain`.
    pub fn compile(&self) -> Result<Arc<gatel_plan::Plan>, Diagnostics> {
        let snapshot = self.observe()?;
        let spec = gatel_plan::normalize(&snapshot, &self.registry)?;
        Ok(Arc::new(gatel_plan::compile(Arc::new(spec), &self.registry)))
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
