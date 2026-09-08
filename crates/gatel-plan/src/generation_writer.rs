//! Host-minted source capability. Callers cannot select another identity or builder.
use std::sync::{Arc, Weak};

use gatel_model::{Fingerprint, Node, ObservationError, SourceId, SourceSnapshot};

use super::{Conflict, GenerationStore, Outcome, Preparation, Registry, Version};

/// A source-scoped synchronous write capability with trusted preparation bound
/// at issuance. Does not expose the store, registry, other source trees or builder.
/// Not authentication, a sandbox, persistence, or host activation. The bound
/// builder must follow normal preparation rules and coordinate its own resources.
pub struct SourceWriter<R, P> {
    store: Weak<GenerationStore<R>>,
    source: SourceId,
    registry: Arc<Registry>,
    prepare: P,
}

impl<R, P> std::fmt::Debug for SourceWriter<R, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceWriter")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceWriteError {
    Unavailable,
    /// Host persistence failed with an indeterminate result. Memory may already
    /// reflect this request; do not interpret this as a failed precondition or
    /// an unapplied write, and do not blindly retry. The plan-only writer never
    /// emits this variant; durable hosts must resolve their own storage state.
    PersistenceUnknown,
    Conflict(Conflict),
    Observation(ObservationError),
}
impl std::fmt::Display for SourceWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("configuration writer is unavailable"),
            Self::PersistenceUnknown => {
                f.write_str("configuration persistence outcome is unknown; memory may already be updated")
            }
            Self::Conflict(error) => error.fmt(f),
            Self::Observation(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SourceWriteError {}

impl<R> GenerationStore<R> {
    /// Whether an identity was explicitly registered at construction. This is
    /// immutable metadata, not a write permit or a check that the store is open.
    pub fn is_registered_source(&self, source: &SourceId) -> bool {
        self.allowed_sources
            .as_ref()
            .is_some_and(|allowed| allowed.contains(source))
    }

    /// Only explicitly registered sources can receive a capability; an open
    /// legacy store is not implicitly an authority to issue arbitrary writers.
    /// Issuance itself does not observe a source or consume its revision.
    pub fn source_writer<P>(
        self: &Arc<Self>,
        source: SourceId,
        registry: Arc<Registry>,
        prepare: P,
    ) -> Option<SourceWriter<R, P>>
    where
        P: Fn(Preparation<'_, R>) -> Result<R, String> + Send + Sync,
    {
        if self.is_sealed() || !self.is_registered_source(&source) {
            return None;
        }
        Some(SourceWriter {
            store: Arc::downgrade(self),
            source,
            registry,
            prepare,
        })
    }
}

impl<R: crate::publication::PublishRuntime, P> SourceWriter<R, P>
where
    P: Fn(Preparation<'_, R>) -> Result<R, String> + Send + Sync,
{
    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// Replace this source's entire contribution. The trusted frontend supplies
    /// identity covering all semantic input; empty nodes retain a tombstone.
    /// The store assigns the revision from latest admitted intent and rechecks
    /// the whole Version after composition. No automatic conflict retries.
    /// Frontends must bound input and filter diagnostics before exposing them.
    pub fn attempt_if(
        &self,
        expected: &Version,
        content: Fingerprint,
        nodes: Vec<Node>,
    ) -> Result<Outcome, SourceWriteError> {
        let store = self.store.upgrade().ok_or(SourceWriteError::Unavailable)?;
        if store.is_sealed() {
            return Err(SourceWriteError::Unavailable);
        }
        let (actual, mut intent) = store.observed_snapshot();
        if &actual != expected {
            return Err(SourceWriteError::Conflict(Conflict { actual }));
        }
        let observed = SourceSnapshot::observe(self.source.clone(), content, nodes, intent.get(&self.source))
            .map_err(SourceWriteError::Observation)?;
        intent.insert(observed);
        store
            .attempt_if(expected, &intent, &self.registry, |preparation| {
                (self.prepare)(preparation)
            })
            .map_err(SourceWriteError::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Hasher, Snapshot, SourceRev};

    use super::*;
    use crate::generation::tests::{prepare_ok, registry, snapshot};

    fn input(text: &str) -> (Fingerprint, Vec<Node>) {
        let mut hash = Hasher::new();
        hash.update_str(text);
        (hash.finish(), gatel_kdl::lower(text, "writer-fixture").unwrap())
    }

    #[test]
    fn scoped_writer_preserves_file_intent_and_revisions_across_rejection_and_removal() {
        let store = Arc::new(GenerationStore::with_allowed_sources([
            SourceId::file("main"),
            SourceId::admin(),
        ]));
        let registry = Arc::new(registry());
        store.attempt(&snapshot(1, "file", 8080), &registry, prepare_ok);
        let original = store
            .observed_snapshot()
            .1
            .get(&SourceId::file("main"))
            .unwrap()
            .clone();
        let writer = store
            .source_writer(SourceId::admin(), registry.clone(), prepare_ok)
            .unwrap();
        assert_eq!(writer.source(), &SourceId::admin());
        let initial = store.status().version();
        // A root namespace resource cannot be claimed through the admin handle.
        let (hash, nodes) = input("upstream \"stolen\" { endpoint \"127.0.0.1:1\"; }");
        assert!(matches!(
            writer.attempt_if(&initial, hash, nodes),
            Ok(Outcome::Rejected(_))
        ));
        let failed = store.status().version();
        let (hash, nodes) = input("upstream \"runtime/shared\" { endpoint \"127.0.0.1:1\"; }");
        assert!(matches!(
            writer.attempt_if(&initial, hash, nodes.clone()),
            Err(SourceWriteError::Conflict(_))
        ));
        assert!(matches!(
            writer.attempt_if(&failed, hash, nodes.clone()),
            Ok(Outcome::Reloaded { .. })
        ));
        let version = store.status().version();
        assert!(matches!(
            writer.attempt_if(&version, hash, nodes),
            Ok(Outcome::NoOp { .. })
        ));
        assert_eq!(store.status().version(), version);
        let (hash, nodes) = input("");
        assert!(matches!(
            writer.attempt_if(&version, hash, nodes),
            Ok(Outcome::Reloaded { .. })
        ));
        let (_, intent) = store.observed_snapshot();
        let file = intent.get(&SourceId::file("main")).unwrap();
        assert_eq!(file.rev, original.rev);
        assert_eq!(file.content, original.content);
        assert_eq!(file.nodes, original.nodes);
        let admin = intent.get(&SourceId::admin()).unwrap();
        assert_eq!(admin.rev, SourceRev(3));
        assert!(admin.nodes.is_empty());
    }

    #[test]
    fn concurrent_users_share_the_bound_builder_and_whole_version_precondition() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = Arc::new(GenerationStore::with_allowed_sources([
            SourceId::file("main"),
            SourceId::admin(),
        ]));
        let registry = Arc::new(registry());
        store.attempt(&snapshot(1, "file", 8080), &registry, prepare_ok);
        let preparations = Arc::new(AtomicUsize::new(0));
        let count = preparations.clone();
        let writer = Arc::new(
            store
                .source_writer(SourceId::admin(), registry, move |preparation| {
                    count.fetch_add(1, Ordering::Relaxed);
                    prepare_ok(preparation)
                })
                .unwrap(),
        );
        let version = store.status().version();
        let start = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for name in ["one", "two"] {
                let writer = writer.clone();
                let version = &version;
                let start = &start;
                workers.push(scope.spawn(move || {
                    let (hash, nodes) = input(&format!(
                        "upstream \"runtime/{name}\" {{ endpoint \"127.0.0.1:1\"; }}"
                    ));
                    start.wait();
                    writer.attempt_if(version, hash, nodes)
                }));
            }
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(Outcome::Reloaded { .. })))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SourceWriteError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            store.observed_snapshot().1.get(&SourceId::admin()).unwrap().rev,
            SourceRev(1)
        );
    }

    #[test]
    fn reused_fingerprint_cannot_publish_different_nodes_at_the_same_revision() {
        let store = Arc::new(GenerationStore::with_allowed_sources([
            SourceId::file("main"),
            SourceId::admin(),
        ]));
        let registry = Arc::new(registry());
        store.attempt(&snapshot(1, "file", 8080), &registry, prepare_ok);
        let writer = store
            .source_writer(SourceId::admin(), registry, prepare_ok)
            .unwrap();
        let (hash, nodes) = input("upstream \"runtime/shared\" { endpoint \"127.0.0.1:1\"; }");
        writer
            .attempt_if(&store.status().version(), hash, nodes.clone())
            .unwrap();
        let version = store.status().version();
        let active = store.current().unwrap();
        let (_, changed) = input("upstream \"runtime/shared\" { endpoint \"127.0.0.1:2\"; }");
        assert!(matches!(
            writer.attempt_if(&version, hash, changed),
            Err(SourceWriteError::Observation(_))
        ));
        assert_eq!(store.status().version(), version);
        assert!(Arc::ptr_eq(&active, &store.current().unwrap()));
        assert_eq!(
            store.observed_snapshot().1.get(&SourceId::admin()).unwrap().nodes,
            nodes
        );
    }

    #[test]
    fn issuance_requires_registration_and_handles_do_not_keep_the_store_alive() {
        let registry = Arc::new(registry());
        let open = Arc::new(GenerationStore::<usize>::new());
        assert!(
            open.source_writer(SourceId::admin(), registry.clone(), prepare_ok)
                .is_none()
        );
        let store = Arc::new(GenerationStore::<usize>::with_allowed_sources(
            [SourceId::admin()],
        ));
        assert!(
            store
                .source_writer(SourceId::file("main"), registry.clone(), prepare_ok)
                .is_none()
        );
        let writer = store
            .source_writer(SourceId::admin(), registry.clone(), prepare_ok)
            .unwrap();
        assert!(store.observed_snapshot().1.is_empty());
        let version = store.status().version();
        let weak = Arc::downgrade(&store);
        drop(store);
        assert!(weak.upgrade().is_none());
        let (hash, nodes) = input("");
        assert!(matches!(
            writer.attempt_if(&version, hash, nodes),
            Err(SourceWriteError::Unavailable)
        ));
    }

    #[test]
    fn exhausted_revision_and_shutdown_cannot_invoke_preparation() {
        let store = Arc::new(GenerationStore::<usize>::with_allowed_sources(
            [SourceId::admin()],
        ));
        let registry = Arc::new(registry());
        let (hash, nodes) = input("");
        store.attempt(
            &Snapshot::new().with(SourceSnapshot::new(
                SourceId::admin(),
                SourceRev(u64::MAX),
                hash,
                nodes,
            )),
            &registry,
            prepare_ok,
        );
        let writer = store
            .source_writer(SourceId::admin(), registry.clone(), |_| {
                panic!("unexpected preparation")
            })
            .unwrap();
        let version = store.status().version();
        let (hash, nodes) = input("nonsense;");
        assert!(matches!(
            writer.attempt_if(&version, hash, nodes.clone()),
            Err(SourceWriteError::Observation(ObservationError::RevisionExhausted))
        ));
        assert_eq!(store.status().version(), version);
        store.seal();
        assert!(
            store
                .source_writer(SourceId::admin(), registry, prepare_ok)
                .is_none()
        );
        assert!(matches!(
            writer.attempt_if(&version, hash, nodes),
            Err(SourceWriteError::Unavailable)
        ));
    }
}
