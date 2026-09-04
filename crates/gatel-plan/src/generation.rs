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
    pub committed_at: Instant,
}

impl<R> Committed<R> {
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
    /// A source moved while this candidate was compiling. The caller should
    /// observe again rather than publish something already out of date.
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
}

/// Why the last attempt failed, kept so readiness can distinguish "never
/// worked" from "still serving the last good configuration".
#[derive(Debug, Clone)]
pub struct Failure {
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
    pub uptime: Option<Duration>,
}

impl Status {
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
    boot_id: BootId,
    generation: Generation,
    current: Option<Arc<Committed<R>>>,
    /// The newest source revisions anyone has observed, which is what makes a
    /// slow candidate detect that it was overtaken.
    head: RevVector,
    last_failure: Option<Failure>,
}

/// Holds the current generation and serializes commits.
pub struct GenerationStore<R> {
    state: RwLock<State<R>>,
}

impl<R> Default for GenerationStore<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> GenerationStore<R> {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(State {
                boot_id: BootId::generate(),
                generation: Generation::NONE,
                current: None,
                head: RevVector::default(),
                last_failure: None,
            }),
        }
    }

    pub fn boot_id(&self) -> BootId {
        self.read().boot_id
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
    {
        // Publishing the observation first is what makes staleness detectable:
        // a candidate that started earlier will find the head has moved past
        // the revisions it was built from.
        let (expected_head, base_generation, previous) = {
            let mut state = self.write();
            state.head.merge_max(&snapshot.revs());
            (state.head.clone(), state.generation, state.current.clone())
        };

        let spec = match normalize(snapshot, registry) {
            Ok(spec) => Arc::new(spec),
            Err(diagnostics) => {
                self.record_failure("normalize", format!("{} problems", diagnostics.error_count()));
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
        {
            let mut state = self.write();
            if state.head != expected_head {
                return Outcome::Stale(stale_reason(&expected_head, &state.head));
            }
            state.last_failure = None;
            return Outcome::NoOp {
                generation: state.generation,
            };
        }

        let Some(next_generation) = base_generation.next() else {
            return Outcome::PrepareFailed("generation counter exhausted".to_string());
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
                self.record_failure("prepare", message.clone());
                return Outcome::PrepareFailed(message);
            }
        };

        let mut state = self.write();
        if state.head != expected_head {
            // `runtime` is dropped here, which is what releases whatever
            // preparation acquired.
            return Outcome::Stale(stale_reason(&expected_head, &state.head));
        }
        if state.generation != base_generation {
            return Outcome::Stale(format!(
                "generation moved from {base_generation} to {} while preparing",
                state.generation
            ));
        }

        let first = state.current.is_none();
        state.generation = next_generation;
        state.current = Some(Arc::new(Committed {
            number: next_generation,
            boot_id: state.boot_id,
            plan,
            runtime,
            revs: snapshot.revs(),
            committed_at: Instant::now(),
        }));
        state.last_failure = None;
        drop(state);

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

    fn record_failure(&self, stage: &'static str, summary: String) {
        let mut state = self.write();
        state.last_failure = Some(Failure {
            stage,
            summary,
            at: Instant::now(),
        });
    }
}

fn stale_reason(expected: &RevVector, actual: &RevVector) -> String {
    expected
        .first_difference(actual)
        .unwrap_or_else(|| "a source was observed again while preparing".to_string())
}

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

        fn build(&self) -> Result<Arc<dyn Handler>, String> {
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

    fn registry() -> Registry {
        Registry::builder()
            .bundle(Bundle::new("std", "1").with(Respond(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap()
    }

    /// Build a snapshot from KDL-shaped nodes without going through a file.
    fn snapshot(rev: u64, body: &str, port: u16) -> Snapshot {
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

    fn invalid_snapshot(rev: u64) -> Snapshot {
        let origin = Origin::synthetic("test");
        Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(rev),
            gatel_model::Hasher::new().finish(),
            vec![Node::new("nonsense", origin)],
        ))
    }

    fn store() -> GenerationStore<usize> {
        GenerationStore::new()
    }

    fn prepare_ok(preparation: Preparation<'_, usize>) -> Result<usize, String> {
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
