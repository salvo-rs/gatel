//! Readable latest intent is distinct from the last known good runtime.
use gatel_model::SourceSnapshot;

use super::{Conflict, GenerationStore, Outcome, Preparation, Registry, Snapshot, Version};

impl<R> GenerationStore<R> {
    /// Record a frontend read/parse failure only against the exact captured
    /// version. No source tree/revision is invented for input that could not be
    /// observed. This does not fence candidates at the same unchanged version.
    pub fn reject_observation_if(
        &self,
        expected: &Version,
        diagnostics: gatel_model::Diagnostics,
    ) -> Result<Outcome, Conflict> {
        let summary = diagnostics
            .iter()
            .next()
            .map(|item| item.message.clone())
            .unwrap_or_else(|| "configuration observation failed".into());
        let mut state = self.write();
        if state.sealed {
            return Ok(Outcome::Stale("configuration publication is sealed".into()));
        }
        let actual = Version {
            boot_id: state.boot_id,
            generation: state.generation,
            revs: state.head.clone(),
        };
        if &actual != expected {
            return Err(Conflict { actual });
        }
        state.last_failure = Some(super::Failure {
            attempts: state
                .last_failure
                .as_ref()
                .map_or(1, |failure| failure.attempts.saturating_add(1)),
            stage: "observe",
            summary,
            at: std::time::Instant::now(),
        });
        Ok(Outcome::Rejected(diagnostics))
    }

    /// Atomically capture the latest admitted source union and its write version.
    /// Includes semantically rejected intent and empty-source tombstones, not
    /// just the configuration serving traffic. Configuration may contain secrets;
    /// expose this only to authorized readers, never as a public status payload.
    /// Trees are copied after releasing the store lock. This is not persistence.
    pub fn observed_snapshot(&self) -> (Version, Snapshot) {
        let (version, sources) = {
            let state = self.read();
            let version = Version {
                boot_id: state.boot_id,
                generation: state.generation,
                revs: state.head.clone(),
            };
            (version, state.intent.values().cloned().collect::<Vec<_>>())
        };
        let mut snapshot = Snapshot::new();
        for source in sources {
            snapshot.insert((*source).clone());
        }
        (version, snapshot)
    }
}

impl<R: crate::publication::PublishRuntime> GenerationStore<R> {
    /// Replace one authorized frontend's complete contribution, preserving all
    /// other latest intents (including rejected ones). Callers assign its revision
    /// and complete content fingerprint. Empty nodes remove contributions without
    /// forgetting the source. Authorization and payload bounds belong to the host.
    /// The ordinary transaction rechecks Version after composition; this is not a
    /// retry/last-writer-wins API and does not activate host workers.
    pub fn attempt_source_if<P>(
        &self,
        expected: &Version,
        source: SourceSnapshot,
        registry: &Registry,
        prepare: P,
    ) -> Result<Outcome, Conflict>
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
    {
        let (actual, mut snapshot) = self.observed_snapshot();
        if &actual != expected {
            return Err(Conflict { actual });
        }
        snapshot.insert(source);
        self.attempt_if(expected, &snapshot, registry, prepare)
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Hasher, Node, Origin, SourceId, SourceRev};

    use super::*;
    use crate::generation::tests::{invalid_snapshot, prepare_ok, registry, snapshot, store};

    fn admin(rev: u64, invalid: bool) -> SourceSnapshot {
        let mut hash = Hasher::new();
        hash.update_u64(u64::from(invalid));
        SourceSnapshot::new(
            SourceId::admin(),
            SourceRev(rev),
            hash.finish(),
            if invalid {
                vec![Node::new("nonsense", Origin::synthetic("intent"))]
            } else {
                vec![]
            },
        )
    }

    #[test]
    fn observation_failures_are_conditional_and_do_not_invent_source_revisions() {
        let store = store();
        let expected = store.status().version();
        for attempts in 1..=2 {
            assert!(matches!(
                store.reject_observation_if(&expected, gatel_model::Diagnostics::new()),
                Ok(Outcome::Rejected(_))
            ));
            assert_eq!(store.status().version(), expected);
            let failure = store.status().last_failure.unwrap();
            assert_eq!(failure.stage, "observe");
            assert_eq!(failure.attempts, attempts);
            assert!(store.observed_snapshot().1.is_empty());
        }
        store.attempt(&snapshot(1, "healthy", 8080), &registry(), prepare_ok);
        let current = store.status().version();
        assert!(
            store
                .reject_observation_if(&expected, gatel_model::Diagnostics::new())
                .is_err()
        );
        assert!(store.status().last_failure.is_none());
        store.seal();
        assert!(matches!(
            store.reject_observation_if(&current, gatel_model::Diagnostics::new()),
            Ok(Outcome::Stale(_))
        ));
        assert!(store.status().last_failure.is_none());
    }

    #[test]
    fn source_updates_preserve_rejected_intent_until_that_source_is_corrected() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
        let serving = store.current().unwrap();
        let version = store.status().version();
        assert!(matches!(
            store.attempt_source_if(&version, admin(1, true), &registry, prepare_ok),
            Ok(Outcome::Rejected(_))
        ));
        let (failed, intent) = store.observed_snapshot();
        assert_eq!(intent.revs(), failed.revs);
        assert_eq!(intent.get(&SourceId::admin()).unwrap().nodes[0].name, "nonsense");
        let file = snapshot(2, "second", 8080).sources().next().unwrap().clone();
        assert!(
            store
                .attempt_source_if(&version, file.clone(), &registry, |_| panic!("obsolete prepare"))
                .is_err()
        );
        assert!(matches!(
            store.attempt_source_if(&failed, file, &registry, prepare_ok),
            Ok(Outcome::Rejected(_))
        ));
        assert!(std::sync::Arc::ptr_eq(&serving, &store.current().unwrap()));
        let (version, intent) = store.observed_snapshot();
        assert_eq!(intent.get(&SourceId::file("main")).unwrap().rev, SourceRev(2));
        assert_eq!(intent.get(&SourceId::admin()).unwrap().rev, SourceRev(1));
        assert!(matches!(
            store.attempt_source_if(&version, admin(2, false), &registry, prepare_ok),
            Ok(Outcome::Reloaded { .. })
        ));
        let (version, intent) = store.observed_snapshot();
        assert_eq!(intent.revs(), version.revs);
        assert_eq!(intent.sources().count(), 2);
        assert!(intent.get(&SourceId::admin()).unwrap().nodes.is_empty());
        assert_eq!(store.current().unwrap().revs, intent.revs());
        // A retained empty source cannot be resurrected by an old observation.
        assert!(matches!(
            store.attempt_source_if(&version, admin(1, true), &registry, |_| panic!("old prepare")),
            Ok(Outcome::Stale(_))
        ));
        assert_eq!(
            store.observed_snapshot().1.get(&SourceId::admin()).unwrap().rev,
            SourceRev(2)
        );
    }

    #[test]
    fn overtaken_preparation_cannot_revert_intent_or_replace_newer_failure() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        for late_failure in [false, true] {
            let store = Arc::new(store());
            let registry = registry();
            store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
            let serving = store.current().unwrap();
            let expected = store.status().version();
            let (entered, preparing) = mpsc::channel();
            let (release, released) = mpsc::channel();
            std::thread::scope(|scope| {
                let worker_store = store.clone();
                let registry = &registry;
                let worker = scope.spawn(move || {
                    let source = snapshot(2, "pending", 8080).sources().next().unwrap().clone();
                    worker_store.attempt_source_if(&expected, source, registry, |preparation| {
                        entered.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(5)).unwrap();
                        if late_failure {
                            Err("obsolete preparation failure".into())
                        } else {
                            prepare_ok(preparation)
                        }
                    })
                });
                preparing.recv_timeout(Duration::from_secs(5)).unwrap();
                let (during, captured) = store.observed_snapshot();
                assert_eq!(during.revs, captured.revs());
                assert_eq!(captured.get(&SourceId::file("main")).unwrap().rev, SourceRev(2));
                assert!(matches!(
                    store.attempt_source_if(&during, admin(1, true), registry, prepare_ok),
                    Ok(Outcome::Rejected(_))
                ));
                let newer = store.status();
                let failure = newer.last_failure.unwrap();
                release.send(()).unwrap();
                assert!(matches!(worker.join().unwrap(), Ok(Outcome::Stale(_))));
                let (version, intent) = store.observed_snapshot();
                assert_eq!(version.revs, intent.revs());
                assert_eq!(intent.get(&SourceId::file("main")).unwrap().rev, SourceRev(2));
                assert_eq!(intent.get(&SourceId::admin()).unwrap().rev, SourceRev(1));
                assert!(Arc::ptr_eq(&serving, &store.current().unwrap()));
                let actual = store.status().last_failure.unwrap();
                assert_eq!(actual.stage, failure.stage);
                assert_eq!(actual.summary, failure.summary);
                assert_eq!(actual.at, failure.at);
                assert!(captured.get(&SourceId::admin()).is_none());
            });
        }
    }

    #[test]
    fn malformed_mixed_observation_does_not_replace_any_retained_tree() {
        let store = store();
        let registry = registry();
        store.attempt(
            &snapshot(1, "first", 8080).with(admin(1, false)),
            &registry,
            prepare_ok,
        );
        let (before, captured) = store.observed_snapshot();
        // A new file revision cannot be retained when another source illegally
        // changes its content identity at the same revision.
        let malformed = invalid_snapshot(2).with(admin(1, true));
        assert!(matches!(
            store.attempt(&malformed, &registry, |_| panic!("malformed prepare")),
            Outcome::Stale(_)
        ));
        let (after, retained) = store.observed_snapshot();
        assert_eq!(before, after);
        for source in captured.sources() {
            let actual = retained.get(&source.source).unwrap();
            assert_eq!(source.rev, actual.rev);
            assert_eq!(source.content, actual.content);
            assert_eq!(source.nodes, actual.nodes);
        }
        store
            .attempt_source_if(&after, admin(2, true), &registry, prepare_ok)
            .unwrap();
        // Captures are independent points in time, not aliases to mutable state.
        assert!(captured.get(&SourceId::admin()).unwrap().nodes.is_empty());
        assert!(
            !store
                .observed_snapshot()
                .1
                .get(&SourceId::admin())
                .unwrap()
                .nodes
                .is_empty()
        );
    }

    #[test]
    fn partial_stale_union_retains_only_newer_intent_and_seal_fences_changes() {
        let store = store();
        let registry = registry();
        store.attempt(
            &snapshot(1, "first", 8080).with(admin(1, false)),
            &registry,
            prepare_ok,
        );
        assert!(matches!(
            store.attempt(&invalid_snapshot(2), &registry, |_| panic!("partial prepare")),
            Outcome::Stale(_)
        ));
        let (version, intent) = store.observed_snapshot();
        assert_eq!(version.revs, intent.revs());
        assert_eq!(
            intent.get(&SourceId::file("main")).unwrap().nodes[0].name,
            "nonsense"
        );
        assert_eq!(intent.get(&SourceId::admin()).unwrap().rev, SourceRev(1));
        store.seal();
        assert!(matches!(
            store.attempt_source_if(&version, admin(2, true), &registry, |_| panic!("sealed prepare")),
            Ok(Outcome::Stale(_))
        ));
        assert_eq!(store.observed_snapshot().0, version);
        assert_eq!(
            store.observed_snapshot().1.get(&SourceId::admin()).unwrap().rev,
            SourceRev(1)
        );
    }
}
