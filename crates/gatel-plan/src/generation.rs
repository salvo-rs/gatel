//! The configuration transaction.
//!
//! Every reload runs the same sequence: observe, normalize, compile, decide
//! whether anything changed, prepare, commit. Only the last step takes the
//! write lock, and only it can fail without leaving the running configuration
//! exactly as it was.
//!
//! The expensive work happens outside the lock, so a slow certificate load
//! cannot stall requests. Correctness comes from re-checking, under the lock,
//! the two things the candidate assumed: that no source moved, and that no
//! other generation was committed in the meantime.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use gatel_model::{BootId, Diagnostics, Fingerprint, Generation, RevVector, Snapshot};

use crate::directive::Registry;
use crate::normalize::normalize;
use crate::plan::{BindingChange, Plan, compile, diff_bindings};

#[path = "generation_version.rs"]
mod version;

struct CandidateBase<R> {
    expected_head: RevVector,
    base_generation: Generation,
    previous: Option<Arc<Committed<R>>>,
    refresh: bool,
}

/// A committed generation: an immutable plan plus whatever the host built from
/// it. A connection pins one of these when it is accepted and keeps it until it
/// closes, so a reload never changes what an in-flight request sees.
pub struct Committed<R> {
    pub number: Generation,
    pub boot_id: BootId,
    pub plan: Arc<Plan>,
    /// Sockets, handler graphs, TLS configuration: opaque here on purpose.
    pub runtime: R,
    /// The source revisions this generation was built from.
    pub revs: RevVector,
    /// Exact compilation input, not latest observed (possibly rejected) intent.
    /// Private to prevent replacing provenance separately from runtime creation.
    sources: Arc<Snapshot>,
    pub committed_at: Instant,
}

impl<R> Committed<R> {
    /// Immutable source union used to build this runtime. May contain secrets;
    /// trusted recovery/inspection only, never a public status response.
    /// NoOp does not replace it. This proves publication provenance, not host
    /// activation, disk durability, raw file contents or external resource state.
    pub fn source_snapshot(&self) -> &Snapshot {
        &self.sources
    }

    pub fn spec_hash(&self) -> Fingerprint {
        self.plan.spec_hash()
    }

    pub fn plan_hash(&self) -> Fingerprint {
        self.plan.plan_hash()
    }
}

impl<R> std::fmt::Debug for Committed<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Committed")
            .field("generation", &self.number)
            .field("plan_hash", &self.plan.plan_hash().short())
            .finish()
    }
}

/// What one attempt did.
#[derive(Debug)]
pub enum Outcome {
    /// The first generation is live.
    Started { generation: Generation },
    /// A new generation replaced the previous one.
    Reloaded {
        generation: Generation,
        changes: Vec<BindingChange>,
    },
    /// Nothing about the running behaviour would differ. No generation was
    /// consumed and no state was touched.
    NoOp { generation: Generation },
    /// The configuration is invalid. The previous generation still serves.
    Rejected(Diagnostics),
    /// Preparing resources failed — a port in use, a certificate that will not
    /// load. Nothing was published; the previous generation still serves.
    PrepareFailed(String),
    /// The observation cannot publish: it was overtaken, reused a revision
    /// with changed content, is unregistered, exceeds source capacity, or the
    /// host has permanently sealed publication for shutdown.
    /// Observe current intent again; identity/capacity violations require fixing
    /// the producer or host limits, not retrying the same candidate in a loop.
    /// A sealed store cannot resume publication; create a new host lifetime.
    Stale(String),
}

impl Outcome {
    pub fn generation(&self) -> Option<Generation> {
        match self {
            Outcome::Started { generation }
            | Outcome::Reloaded { generation, .. }
            | Outcome::NoOp { generation } => Some(*generation),
            _ => None,
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Outcome::Rejected(_) | Outcome::PrepareFailed(_))
    }

    /// Publish what this transaction did.
    ///
    /// The counter and readiness are told separately, because they answer
    /// different questions: one says what happened, the other whether this
    /// instance should still be sent traffic. A refused configuration moves the
    /// first and deliberately does not remove the instance — every replica
    /// reads the same file, and each of them is still serving the last one that
    /// worked.
    pub fn record(&self, telemetry: &gatel_observe::Telemetry) {
        telemetry.metrics.process.reloaded(match self {
            Outcome::Started { .. } => gatel_observe::Reload::Started,
            Outcome::Reloaded { .. } => gatel_observe::Reload::Reloaded,
            Outcome::NoOp { .. } => gatel_observe::Reload::NoOp,
            Outcome::Rejected(_) => gatel_observe::Reload::Rejected,
            Outcome::PrepareFailed(_) => gatel_observe::Reload::PrepareFailed,
            Outcome::Stale(_) => gatel_observe::Reload::Stale,
        });
        if let Some(generation) = self.generation() {
            telemetry.metrics.process.generation.observe(generation.number());
        }
        match self {
            Outcome::Rejected(diagnostics) => telemetry.health.reload_failed(
                diagnostics
                    .iter()
                    .next()
                    .map(|first| first.message.clone())
                    .unwrap_or_else(|| "invalid configuration".to_string()),
            ),
            Outcome::PrepareFailed(detail) => telemetry.health.reload_failed(detail.clone()),
            // A stale candidate is not a failure: something newer was observed,
            // and that one reports for itself.
            _ => {}
        }
    }
}

/// Why the last attempt failed, kept so readiness can distinguish "never
/// worked" from "still serving the last good configuration".
#[derive(Debug, Clone)]
pub struct Failure {
    pub attempts: u32,
    pub stage: &'static str,
    pub summary: String,
    pub at: Instant,
}

/// Everything the operations surface needs to know.
#[derive(Debug, Clone)]
pub struct Status {
    pub boot_id: BootId,
    pub generation: Generation,
    pub spec_hash: Option<Fingerprint>,
    pub plan_hash: Option<Fingerprint>,
    pub revs: RevVector,
    pub last_failure: Option<Failure>,
    /// Latest admitted revisions differ from the published build input. False
    /// before publication. This includes NoOp and in-flight intent, not just
    /// rejected changes, and does not imply a different effective plan.
    pub observed_differs: bool,
    pub uptime: Option<Duration>,
}

/// Optimistic write precondition for one store lifetime and observed intent.
/// Includes the observed head: rejected and no-op revisions also invalidate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub boot_id: BootId,
    pub generation: Generation,
    pub revs: RevVector,
}

/// A failed precondition is not a configuration failure and mutates nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub actual: Version,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("configuration write precondition no longer matches")
    }
}
impl std::error::Error for Conflict {}

impl Status {
    pub fn version(&self) -> Version {
        Version {
            boot_id: self.boot_id,
            generation: self.generation,
            revs: self.revs.clone(),
        }
    }

    /// Serving traffic, whether or not the newest configuration was accepted.
    pub fn is_serving(&self) -> bool {
        self.generation != Generation::NONE
    }

    /// Serving, but the operator's newest intent was refused.
    pub fn is_degraded(&self) -> bool {
        self.is_serving() && self.last_failure.is_some()
    }
}

/// What `prepare` is given.
pub struct Preparation<'a, R> {
    pub plan: &'a Arc<Plan>,
    /// The generation currently serving, if any, so preparation can reuse its
    /// sockets and state.
    pub previous: Option<&'a Committed<R>>,
    /// What happens to each piece of state if this candidate commits.
    pub changes: &'a [BindingChange],
    /// The number this generation will get if it commits.
    pub generation: Generation,
    pub boot_id: BootId,
}

struct State<R> {
    sealed: bool,
    boot_id: BootId,
    generation: Generation,
    current: Option<Arc<Committed<R>>>,
    /// The newest source revisions anyone has observed, which is what makes a
    /// slow candidate detect that it was overtaken.
    head: RevVector,
    /// Latest admitted source intent, including failed configuration. Arc keeps
    /// readers and replacement cleanup from cloning/dropping trees under locks.
    intent: std::collections::BTreeMap<gatel_model::SourceId, Arc<gatel_model::SourceSnapshot>>,
    last_failure: Option<Failure>,
}

impl<R> State<R> {
    fn validate_candidate(&self, expected: &RevVector, base: Generation) -> Result<(), Outcome> {
        if self.sealed {
            return Err(Outcome::Stale("configuration publication is sealed".into()));
        }
        if self.head != *expected {
            return Err(Outcome::Stale(stale_reason(expected, &self.head)));
        }
        if self.generation != base {
            return Err(Outcome::Stale(format!(
                "generation moved from {base} to {} while preparing",
                self.generation
            )));
        }
        Ok(())
    }
}

#[path = "generation_observe.rs"]
mod observation;

#[path = "generation_intent.rs"]
mod intent;

#[path = "generation_capture.rs"]
mod capture;
pub use capture::ConfigurationCapture;

#[path = "generation_restore.rs"]
mod restore;
pub use restore::RestoreIntentError;

#[path = "generation_writer.rs"]
mod writer;
pub use writer::{SourceWriteError, SourceWriter};

/// Holds the current generation and serializes commits.
pub struct GenerationStore<R> {
    state: RwLock<State<R>>,
    source_limit: std::num::NonZeroUsize,
    allowed_sources: Option<std::collections::BTreeSet<gatel_model::SourceId>>,
}

impl<R> Default for GenerationStore<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> GenerationStore<R> {
    pub fn new() -> Self {
        Self::with_source_limit(std::num::NonZeroUsize::new(256).expect("nonzero default"))
    }

    /// Maximum distinct source identities observed in this store lifetime.
    /// Empty tombstones still consume an identity; dropping history could
    /// allow old revisions to reappear. This does not bound source payloads.
    pub fn with_source_limit(source_limit: std::num::NonZeroUsize) -> Self {
        Self {
            source_limit,
            allowed_sources: None,
            state: RwLock::new(State {
                sealed: false,
                boot_id: BootId::generate(),
                generation: Generation::NONE,
                current: None,
                head: RevVector::default(),
                intent: Default::default(),
                last_failure: None,
            }),
        }
    }

    pub fn boot_id(&self) -> BootId {
        self.read().boot_id
    }

    /// Irreversibly fence new observations and publication, including candidates
    /// already preparing. Existing readers retain the current generation.
    /// This does not cancel/join preparation or drain runtime resources.
    pub fn seal(&self) {
        self.write().sealed = true;
    }

    pub fn is_sealed(&self) -> bool {
        self.read().sealed
    }

    /// Restrict observations to a host-defined source set, fixed for this store
    /// lifetime. Empty means deny all sources. Frontends must not choose their
    /// identity from unauthenticated request data; this is not authentication.
    pub fn with_allowed_sources(sources: impl IntoIterator<Item = gatel_model::SourceId>) -> Self {
        let allowed: std::collections::BTreeSet<_> = sources.into_iter().collect();
        let mut store = Self::with_source_limit(
            std::num::NonZeroUsize::new(allowed.len().max(1)).expect("positive capacity"),
        );
        store.allowed_sources = Some(allowed);
        store
    }

    /// The generation a new connection should pin.
    pub fn current(&self) -> Option<Arc<Committed<R>>> {
        self.read().current.clone()
    }

    pub fn status(&self) -> Status {
        let state = self.read();
        Status {
            boot_id: state.boot_id,
            generation: state.generation,
            spec_hash: state.current.as_ref().map(|committed| committed.spec_hash()),
            plan_hash: state.current.as_ref().map(|committed| committed.plan_hash()),
            revs: state.head.clone(),
            last_failure: state.last_failure.clone(),
            observed_differs: state
                .current
                .as_ref()
                .is_some_and(|current| current.revs != state.head),
            uptime: state
                .current
                .as_ref()
                .map(|committed| committed.committed_at.elapsed()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State<R>> {
        self.state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State<R>> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run one candidate from observation to commit.
    ///
    /// `prepare` does the work that can fail for reasons configuration cannot
    /// predict: binding sockets, loading certificates, constructing handlers.
    /// It runs outside the lock and its result is discarded if the candidate
    /// turns out to be stale, so it must clean up after itself on drop.
    pub fn attempt<P>(&self, snapshot: &Snapshot, registry: &Registry, prepare: P) -> Outcome
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        self.attempt_conditionally(snapshot, registry, None, false, prepare)
            .expect("unconditional attempts cannot fail a precondition")
    }

    /// Check a previously read status version under the observation write lock.
    /// A conflict never observes the supplied revisions or invokes preparation.
    /// Once admitted, normal stale checks still protect preparation and commit.
    /// This is not authorization, source revision allocation, or persistence.
    pub fn attempt_if<P>(
        &self,
        expected: &Version,
        snapshot: &Snapshot,
        registry: &Registry,
        prepare: P,
    ) -> Result<Outcome, Conflict>
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        self.attempt_conditionally(snapshot, registry, Some(expected), false, prepare)
    }

    /// Rebuild external resources even when the compiled plan is unchanged.
    /// Success publishes a new generation; failures retain the current one.
    /// This does not discover changed files or revoke resources held by old
    /// generations. Preparation must still be cancellation/stale-safe.
    pub fn refresh<P>(&self, snapshot: &Snapshot, registry: &Registry, prepare: P) -> Outcome
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        self.attempt_conditionally(snapshot, registry, None, true, prepare)
            .expect("unconditional refresh cannot fail a precondition")
    }

    /// Version-conditional resource refresh, with the same observation and
    /// publication fencing as `attempt_if`.
    pub fn refresh_if<P>(
        &self,
        expected: &Version,
        snapshot: &Snapshot,
        registry: &Registry,
        prepare: P,
    ) -> Result<Outcome, Conflict>
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        self.attempt_conditionally(snapshot, registry, Some(expected), true, prepare)
    }

    fn finish_attempt<P>(
        &self,
        snapshot: &Snapshot,
        registry: &Registry,
        prepare: P,
        base: CandidateBase<R>,
    ) -> Outcome
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        let CandidateBase {
            expected_head,
            base_generation,
            previous,
            refresh,
        } = base;
        let spec = match normalize(snapshot, registry) {
            Ok(spec) => Arc::new(spec),
            Err(diagnostics) => {
                if let Err(stale) = self.record_failure(
                    &expected_head,
                    base_generation,
                    "normalize",
                    format!("{} problems", diagnostics.error_count()),
                ) {
                    return stale;
                }
                return Outcome::Rejected(diagnostics);
            }
        };

        let plan = Arc::new(compile(spec, registry));

        let current_bindings: &[_] = previous
            .as_ref()
            .map(|committed| committed.plan.bindings.as_slice())
            .unwrap_or(&[]);
        let changes = diff_bindings(current_bindings, &plan.bindings);

        // Nothing about the running behaviour would differ, so there is no
        // reason to build anything or consume a generation number.
        if let Some(committed) = &previous
            && committed.plan_hash() == plan.plan_hash()
            && !refresh
        {
            let mut state = self.write();
            if let Err(stale) = state.validate_candidate(&expected_head, base_generation) {
                return stale;
            }
            state.last_failure = None;
            return Outcome::NoOp {
                generation: state.generation,
            };
        }

        let Some(next_generation) = base_generation.next() else {
            let message = "generation counter exhausted".to_string();
            if let Err(stale) =
                self.record_failure(&expected_head, base_generation, "prepare", message.clone())
            {
                return stale;
            }
            return Outcome::PrepareFailed(message);
        };
        let boot_id = { self.read().boot_id };

        let runtime = match prepare(Preparation {
            plan: &plan,
            previous: previous.as_deref(),
            changes: &changes,
            generation: next_generation,
            boot_id,
        }) {
            Ok(runtime) => runtime,
            Err(message) => {
                if let Err(stale) =
                    self.record_failure(&expected_head, base_generation, "prepare", message.clone())
                {
                    return stale;
                }
                return Outcome::PrepareFailed(message);
            }
        };

        // Deep-copy trees outside the generation/publication locks. Pin them
        // with this runtime so later failed intent cannot contaminate recovery
        // input, and old readers retain their own generation's provenance.
        let sources = Arc::new(snapshot.clone());
        let revs = sources.revs();
        let mut state = self.write();
        if let Err(stale) = state.validate_candidate(&expected_head, base_generation) {
            // `runtime` is dropped here, which is what releases whatever
            // preparation acquired.
            return stale;
        }

        let first = state.current.is_none();
        let committed = Arc::new(Committed {
            number: next_generation,
            boot_id: state.boot_id,
            plan,
            runtime,
            revs,
            sources,
            committed_at: Instant::now(),
        });
        // Allocate before locking shared runtime state. Publication itself only
        // swaps owned values; no fallible preparation or plugin callbacks here.
        let publication = committed.runtime.begin_publication();
        state.generation = next_generation;
        state.current = Some(committed.clone());
        state.last_failure = None;
        drop(state);
        crate::publication::PublishGuard::published(publication);
        committed.runtime.finish_publication();

        if first {
            Outcome::Started {
                generation: next_generation,
            }
        } else {
            Outcome::Reloaded {
                generation: next_generation,
                changes,
            }
        }
    }

    fn record_failure(
        &self,
        expected: &RevVector,
        base: Generation,
        stage: &'static str,
        summary: String,
    ) -> Result<(), Outcome> {
        let mut state = self.write();
        state.validate_candidate(expected, base)?;
        state.last_failure = Some(Failure {
            attempts: state
                .last_failure
                .as_ref()
                .map_or(1, |failure| failure.attempts.saturating_add(1)),
            stage,
            summary,
            at: Instant::now(),
        });
        Ok(())
    }
}

fn stale_reason(expected: &RevVector, actual: &RevVector) -> String {
    expected
        .first_difference(actual)
        .unwrap_or_else(|| "a source was observed again while preparing".to_string())
}

#[cfg(test)]
#[path = "generation_races.rs"]
mod races;

#[cfg(test)]
#[path = "generation_preconditions.rs"]
mod preconditions;

#[cfg(test)]
#[path = "generation_refresh.rs"]
mod refresh_tests;

#[cfg(test)]
#[path = "generation_metrics.rs"]
mod metric_tests;

#[cfg(test)]
#[path = "generation_identity.rs"]
mod identity;

#[cfg(test)]
#[path = "generation_finish.rs"]
mod finish_tests;

#[cfg(test)]
#[path = "generation_seal.rs"]
mod seal_tests;

#[cfg(test)]
#[path = "generation_provenance.rs"]
mod provenance_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gatel_model::{Hasher, Node, Origin, Scalar, SourceId, SourceRev, SourceSnapshot, Value};
    use salvo_core::Handler;

    use super::*;
    use crate::directive::{Bundle, Directive, DirectiveConfig, DirectiveMeta, Registry};

    struct Respond(DirectiveMeta);
    struct RespondConfig(String);

    impl DirectiveConfig for RespondConfig {
        fn hash_into(&self, hasher: &mut Hasher) {
            hasher.update_str(&self.0);
        }

        fn build(&self, _cx: crate::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
            Err("not built in tests".into())
        }

        fn describe(&self) -> String {
            format!("respond {:?}", self.0)
        }
    }

    impl Directive for Respond {
        fn meta(&self) -> &DirectiveMeta {
            &self.0
        }

        fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
            let body = node
                .args
                .first()
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            Some(Arc::new(RespondConfig(body)))
        }
    }

    pub(super) fn registry() -> Registry {
        Registry::builder()
            .bundle(Bundle::new("std", "1").with(Respond(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap()
    }

    /// Build a snapshot from KDL-shaped nodes without going through a file.
    pub(super) fn snapshot(rev: u64, body: &str, port: u16) -> Snapshot {
        let origin = Origin::synthetic("test");
        let global = Node::new("global", origin.clone()).with_child(
            Node::new("http", origin.clone())
                .with_arg(Value::string(format!("127.0.0.1:{port}"), origin.clone())),
        );
        let site = Node::new("site", origin.clone())
            .with_arg(Value::string("*", origin.clone()))
            .with_child(
                Node::new("route", origin.clone())
                    .with_arg(Value::string("/", origin.clone()))
                    .with_child(
                        Node::new("respond", origin.clone()).with_arg(Value::string(body, origin.clone())),
                    ),
            );
        let mut hasher = Hasher::new();
        hasher.update_str(body);
        hasher.update_u64(port as u64);
        Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(rev),
            hasher.finish(),
            vec![global, site],
        ))
    }

    pub(super) fn invalid_snapshot(rev: u64) -> Snapshot {
        let origin = Origin::synthetic("test");
        Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(rev),
            gatel_model::Hasher::new().finish(),
            vec![Node::new("nonsense", origin)],
        ))
    }

    pub(super) fn store() -> GenerationStore<usize> {
        GenerationStore::new()
    }

    pub(super) fn prepare_ok(preparation: Preparation<'_, usize>) -> Result<usize, String> {
        Ok(preparation.generation.number() as usize)
    }

    #[test]
    fn a_cold_start_commits_generation_one() {
        let store = store();
        let outcome = store.attempt(&snapshot(1, "hello", 8080), &registry(), prepare_ok);
        assert!(matches!(outcome, Outcome::Started { generation } if generation == Generation::FIRST));
        assert_eq!(store.status().generation, Generation::FIRST);
    }

    #[test]
    fn a_real_change_advances_the_generation() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);
        let outcome = store.attempt(&snapshot(2, "goodbye", 8080), &registry, prepare_ok);
        assert!(matches!(outcome, Outcome::Reloaded { generation, .. } if generation.number() == 2));
    }

    #[test]
    fn an_unchanged_configuration_is_a_noop_and_consumes_nothing() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);

        let calls = AtomicUsize::new(0);
        let outcome = store.attempt(&snapshot(2, "hello", 8080), &registry, |preparation| {
            calls.fetch_add(1, Ordering::SeqCst);
            prepare_ok(preparation)
        });
        assert!(matches!(outcome, Outcome::NoOp { generation } if generation.number() == 1));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a no-op must not prepare anything"
        );
        assert_eq!(store.status().generation, Generation::FIRST);
    }

    #[test]
    fn an_invalid_candidate_leaves_the_previous_generation_serving() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);
        let before = store.current().unwrap();

        let outcome = store.attempt(&invalid_snapshot(2), &registry, prepare_ok);
        assert!(matches!(outcome, Outcome::Rejected(_)));

        let after = store.current().unwrap();
        assert_eq!(after.number, before.number);
        assert_eq!(store.status().generation, Generation::FIRST);
        assert!(
            store.status().is_degraded(),
            "the operator must be able to see the rejection"
        );
    }

    #[test]
    fn a_failed_preparation_does_not_consume_a_generation() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);
        let outcome = store.attempt(&snapshot(2, "goodbye", 8080), &registry, |_| {
            Err("address already in use".to_string())
        });
        assert!(matches!(outcome, Outcome::PrepareFailed(message) if message.contains("in use")));
        assert_eq!(store.status().generation, Generation::FIRST);
    }

    #[test]
    fn a_candidate_overtaken_while_preparing_is_stale() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "a", 8080), &registry, prepare_ok);

        // While this candidate is preparing, a newer observation arrives.
        let outcome = store.attempt(&snapshot(2, "b", 8080), &registry, |preparation| {
            store.attempt(&snapshot(3, "c", 8080), &registry, prepare_ok);
            prepare_ok(preparation)
        });
        assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
        // The newer candidate is the one that is live.
        assert_eq!(store.status().generation.number(), 2);
    }

    #[test]
    fn recovering_after_a_rejection_clears_the_failure() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);
        store.attempt(&invalid_snapshot(2), &registry, prepare_ok);
        assert!(store.status().is_degraded());

        store.attempt(&snapshot(3, "recovered", 8080), &registry, prepare_ok);
        assert!(!store.status().is_degraded());
        assert_eq!(store.status().generation.number(), 2);
    }

    #[test]
    fn a_first_configuration_that_never_works_is_not_serving() {
        let store = store();
        let outcome = store.attempt(&invalid_snapshot(1), &registry(), prepare_ok);
        assert!(matches!(outcome, Outcome::Rejected(_)));
        assert!(!store.status().is_serving());
        assert!(store.current().is_none());
    }

    #[test]
    fn the_previous_generation_is_offered_to_preparation() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);
        store.attempt(&snapshot(2, "goodbye", 8080), &registry, |preparation| {
            let previous = preparation.previous.expect("a reload must see what it replaces");
            assert_eq!(previous.number, Generation::FIRST);
            prepare_ok(preparation)
        });
    }

    #[test]
    fn comment_only_edits_do_not_reach_preparation() {
        // Two snapshots with different content fingerprints but the same
        // meaning: the store must stop at the plan hash, not the file bytes.
        let registry = registry();
        let store = store();
        store.attempt(&snapshot(1, "hello", 8080), &registry, prepare_ok);

        let origin = Origin::synthetic("test");
        let mut reordered = snapshot(2, "hello", 8080);
        let source = reordered.get(&SourceId::file("main")).unwrap().clone();
        let mut nodes = source.nodes.clone();
        nodes.reverse();
        nodes[0].props.push(gatel_model::Property {
            name: "ignored-by-nothing".into(),
            value: Value::new(Scalar::Null, origin.clone()),
            origin,
        });
        nodes[0].props.clear();
        reordered.insert(SourceSnapshot::new(
            source.source,
            SourceRev(2),
            source.content,
            nodes,
        ));

        let outcome = store.attempt(&reordered, &registry, |_| Err("must not prepare".into()));
        assert!(matches!(outcome, Outcome::NoOp { .. }), "{outcome:?}");
    }
}
