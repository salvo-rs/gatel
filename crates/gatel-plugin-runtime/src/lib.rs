//! Version-coupled plugin runtime SDK. No dependency on the compiler or transport.
#![forbid(unsafe_code)]
use std::any::Any;
use std::sync::Arc;

use gatel_model::{Diagnostics, Fingerprint, Hasher, Node, ResourceId};
pub use gatel_plugin_api::{DirectiveMeta, Effect, FailurePolicy, Phase, Role, Scope, Scopes};
use salvo_core::Handler;
pub mod blocking;
mod body_error;
pub mod secrets;
pub mod subrequest;
pub mod task;
pub use body_error::{BodyTooLarge, RequestBodyLimit};
pub use subrequest::{Answer, Ask, SharedUpstreams, UpstreamHandle};
pub use task::{BackgroundTask, Shutdown, ShutdownSignal};

/// A bulk-file response requests an optional host output-queue bound.
/// Hosts that do not support this hint retain their usual output policy.
/// This zero-sized response marker carries no request or connection handle.
#[derive(Debug, Clone, Copy)]
pub struct BoundedFileOutput;

/// Host-supplied identity of the selected execution plan and declared upstream
/// configurations. Not an authorization capability or a generation counter.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionIdentity(pub Fingerprint);
impl ExecutionIdentity {
    pub const DEPOT_KEY: &'static str = "gatel.execution_identity";
}

/// State a directive keeps between requests.
///
/// A directive that needs one declares it here rather than reaching for a
/// global: a rate limiter's buckets must survive a reload that did not change
/// the limit, and must *not* survive one that did.
pub struct DirectiveState {
    /// Changes exactly when the existing buckets or counters stop meaning what
    /// they meant. Raising a window does; renaming an unrelated route does not.
    pub compatibility: Fingerprint,
    /// Builds a fresh instance when the existing one cannot be reused.
    pub create: Box<dyn Fn() -> Arc<dyn Any + Send + Sync> + Send + Sync>,
}

impl std::fmt::Debug for DirectiveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveState")
            .field("compatibility", &self.compatibility.short())
            .finish()
    }
}

/// What a directive is given when its handler is built.
///
/// Everything here is decided before anything is published, so a secret that
/// cannot be read or a path that does not exist stops a reload rather than
/// failing a request later.
#[derive(Clone)]
pub struct BuildContext<'a> {
    /// Observes secrets read by this generation, never part of plan hashes.
    pub secrets: Arc<secrets::SecretDependencies>,
    /// Host-owned blocking admission, shared across generations. Capture the
    /// executor, never create a replacement budget during directive prepare.
    pub blocking: blocking::Executor,
    /// The directory `file:` references resolve against.
    pub root: &'a std::path::Path,
    /// Whatever [`DirectiveConfig::state`] asked for, resolved against what is
    /// already running. `None` for a directive that asked for nothing.
    pub state: Option<Arc<dyn Any + Send + Sync>>,
    /// What the process measures and reports about itself. Process-wide and
    /// older than any generation, like a socket: a directive that exposes it
    /// captures the handle, it is never rebuilt for a reload.
    pub telemetry: &'a Arc<gatel_observe::Telemetry>,
    /// Host-owned candidate log policies, published only with the generation.
    /// Standalone handler builders may omit this and use immediate sinks.
    pub log_sinks: Option<&'a gatel_observe::sink::StagedSinks>,
    /// The backends this generation can reach, for a directive that has to ask
    /// one something. Handed over here and nowhere else, so the set a directive
    /// can reach is fixed when it is built.
    pub upstreams: &'a crate::subrequest::SharedUpstreams,
}

impl BuildContext<'_> {
    /// Read the material a reference names, with the usual bounds and
    /// permission checks.
    pub fn secret(&self, reference: &gatel_model::SecretRef) -> Result<gatel_model::SecretString, String> {
        self.secrets.read(reference, self.root)
    }

    /// The upstream this directive named, checked to exist.
    ///
    /// Compilation already refused an unknown name, so a failure here means the
    /// runtime and the plan disagree — which is worth stopping a reload for
    /// rather than discovering on a request.
    pub fn upstream(&self, upstream: &ResourceId) -> Result<UpstreamHandle, String> {
        if !self.upstreams.has(upstream) {
            return Err(format!("upstream `{}` has no runtime", upstream.name()));
        }
        Ok(UpstreamHandle::new(Arc::clone(self.upstreams), upstream.clone()))
    }

    /// Take the state as a concrete type, or say what went wrong.
    pub fn state<T: Any + Send + Sync>(&self, what: &str) -> Result<Arc<T>, String> {
        self.state
            .clone()
            .ok_or_else(|| format!("{what} was not provided"))?
            .downcast::<T>()
            .map_err(|_| format!("{what} is not of the expected type"))
    }
}

/// One generation-local resource bundle, with inert background factories.
pub struct PreparedDirective {
    pub handler: Arc<dyn Handler>,
    pub tasks: Vec<BackgroundTask>,
}
impl PreparedDirective {
    pub fn new(handler: Arc<dyn Handler>) -> Self {
        Self {
            handler,
            tasks: Vec::new(),
        }
    }
    pub fn with_tasks(mut self, tasks: Vec<BackgroundTask>) -> Self {
        self.tasks = tasks;
        self
    }
}

/// A decoded directive configuration. `hash_into` covers all behavior changes
/// and nothing else, because it determines whether a reload is a no-op.
pub trait DirectiveConfig: Any + Send + Sync + 'static {
    fn hash_into(&self, hasher: &mut Hasher);

    /// Prepare exactly once per owner per candidate generation. Return the
    /// handler and inert task factories together so they can share resources.
    /// The host reuses this handler across inherited routes and listeners.
    fn prepare(&self, cx: BuildContext<'_>) -> Result<PreparedDirective, String> {
        let handler = self.build(cx.clone())?;
        Ok(PreparedDirective::new(handler).with_tasks(self.background(cx)?))
    }

    /// Build the handler for one generation. Called during `prepare`, so a
    /// failure here rejects the candidate and leaves the running configuration
    /// untouched.
    ///
    /// Anything it needs from outside — a secret, a resolved path, its own
    /// state — arrives in the context.
    fn build(&self, _cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Err("directive must implement prepare or build".into())
    }

    /// Describe generation-scoped background work without starting it. The host
    /// invokes each factory only after commit. Dropping a future must release
    /// its resources; spawning untracked child tasks here violates the contract.
    /// Compatible state can survive reload, but these tasks restart per generation.
    fn background(&self, _cx: BuildContext<'_>) -> Result<Vec<BackgroundTask>, String> {
        Ok(Vec::new())
    }

    /// What this directive needs to keep between requests, if anything.
    fn state(&self) -> Option<DirectiveState> {
        None
    }

    /// Upstreams this directive refers to by name.
    ///
    /// Declared so compilation can check the names against what the file
    /// actually declares. A directive that keeps this to itself would turn a
    /// typo into a 502 the first time somebody was refused, at which point the
    /// operator is reading the wrong logs.
    fn upstreams(&self) -> Vec<ResourceId> {
        Vec::new()
    }

    /// One line for `gatel explain`.
    fn describe(&self) -> String;
}

/// A directive implementation: metadata plus a decoder.
pub trait Directive: Send + Sync + 'static {
    fn meta(&self) -> &DirectiveMeta;

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>>;
}

/// A named group of directives, shipped and trimmed as a unit.
pub struct Bundle {
    pub name: &'static str,
    pub version: &'static str,
    pub directives: Vec<Arc<dyn Directive>>,
}

impl Bundle {
    pub fn new(name: &'static str, version: &'static str) -> Self {
        Self {
            name,
            version,
            directives: Vec::new(),
        }
    }

    pub fn with(mut self, directive: impl Directive) -> Self {
        self.directives.push(Arc::new(directive));
        self
    }
}
