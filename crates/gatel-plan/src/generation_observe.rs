//! Source admission and intent observation under the transaction write lock.
//!
//! All frontends share these rules; this layer performs no I/O or preparation
//! while the lock is held. Publication and failure recording remain in generation.
use super::{
    CandidateBase, Conflict, GenerationStore, Outcome, Preparation, Registry, Snapshot, Version, stale_reason,
};

impl<R> GenerationStore<R> {
    pub(super) fn attempt_conditionally<P>(
        &self,
        snapshot: &Snapshot,
        registry: &Registry,
        expected: Option<&Version>,
        refresh: bool,
        prepare: P,
    ) -> Result<Outcome, Conflict>
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        // Publishing the observation first is what makes staleness detectable:
        // a candidate that started earlier will find the head has moved past
        // the revisions it was built from.
        let expected_head = snapshot.revs();
        // Revisions never decrease: a source already observed here cannot
        // become new while we clone outside the lock. A concurrent newer writer
        // can only make this work redundant; the write-lock checks below decide.
        let observed = self.new_intent(snapshot);
        let mut retired = Vec::new();
        let (base_generation, previous) = {
            let mut state = self.write();
            if state.sealed {
                return Ok(Outcome::Stale("configuration publication is sealed".into()));
            }
            if let Some(expected) = expected {
                let actual = Version {
                    boot_id: state.boot_id,
                    generation: state.generation,
                    revs: state.head.clone(),
                };
                if *expected != actual {
                    return Err(Conflict { actual });
                }
            }
            // Validate every equal revision before advancing any source. A
            // malformed mixed-source observation must not partially fence off
            // valid candidates or overwrite the fingerprint of accepted intent.
            if self
                .allowed_sources
                .as_ref()
                .is_some_and(|allowed| snapshot.sources().any(|source| !allowed.contains(&source.source)))
            {
                return Ok(Outcome::Stale(
                    "configuration source is not registered by this host".into(),
                ));
            }
            let new_sources = snapshot
                .sources()
                .filter(|source| state.head.get(&source.source).is_none())
                .count();
            if state.intent.len().saturating_add(new_sources) > self.source_limit.get() {
                return Ok(Outcome::Stale("configuration source capacity exhausted".into()));
            }
            for source in snapshot.sources() {
                if state.head.get(&source.source) == Some(source.rev)
                    && state.intent.get(&source.source).map(|source| source.content) != Some(source.content)
                {
                    return Ok(Outcome::Stale(
                        "source content changed without advancing its revision".into(),
                    ));
                }
            }
            for source in &observed {
                if state.head.get(&source.source).is_none_or(|rev| source.rev > rev) {
                    retired.extend(state.intent.insert(source.source.clone(), source.clone()));
                }
            }
            state.head.merge_max(&expected_head);
            // A late observation cannot borrow the newer merged head as its
            // own revision vector. Snapshot is the complete set of sources.
            if state.head != expected_head {
                return Ok(Outcome::Stale(stale_reason(&expected_head, &state.head)));
            }
            (state.generation, state.current.clone())
        };
        drop(retired);

        Ok(self.finish_attempt(
            snapshot,
            registry,
            prepare,
            CandidateBase {
                expected_head,
                base_generation,
                previous,
                refresh,
            },
        ))
    }

    fn new_intent(&self, snapshot: &Snapshot) -> Vec<std::sync::Arc<gatel_model::SourceSnapshot>> {
        let sources: Vec<_> = {
            let state = self.read();
            snapshot
                .sources()
                .filter(|source| state.head.get(&source.source).is_none_or(|rev| source.rev > rev))
                .collect()
        };
        // No tree copy or destruction under the state lock. Unchanged/stale
        // sources need no duplicate retained tree on every polling reload.
        sources.into_iter().cloned().map(std::sync::Arc::new).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::generation::tests::{prepare_ok, registry, snapshot, store};

    #[test]
    fn retention_copies_only_previously_unseen_revisions() {
        let store = store();
        let first = snapshot(2, "first", 8080);
        assert_eq!(store.new_intent(&first).len(), 1);
        store.attempt(&first, &registry(), prepare_ok);
        assert!(store.new_intent(&first).is_empty());
        assert!(store.new_intent(&snapshot(1, "old", 8080)).is_empty());
        assert_eq!(store.new_intent(&snapshot(3, "new", 8080)).len(), 1);
    }
}
