//! Opaque control-plane write validators, independent of an HTTP framework.
use gatel_model::Hasher;

use super::{Conflict, GenerationStore, Outcome, Preparation, Registry, Snapshot, Version};

impl Version {
    /// A single strong entity tag for configuration write preconditions.
    /// Includes failed/no-op intent revisions and the full boot identity.
    /// It is not a credential or a validator for arbitrary status response bytes.
    pub fn etag(&self) -> String {
        let mut hash = Hasher::new();
        hash.update_str("gatel.configuration-version.v1");
        self.boot_id.hash_into(&mut hash);
        hash.update_u64(self.generation.number());
        for (source, revision) in self.revs.iter() {
            hash.update_str(&source.to_string());
            hash.update_u64(revision.0);
        }
        format!("\"gatel-v1-{}\"", hash.finish())
    }
}

impl<R: crate::publication::PublishRuntime> GenerationStore<R> {
    /// Admit exactly the current tag, then recheck its complete Version under
    /// the ordinary transaction lock. A concurrent observation cannot slip
    /// between this comparison and `attempt_if` and publish an obsolete union.
    ///
    /// This requires one exact strong tag; no weak tags, lists, wildcards or
    /// whitespace normalization. An HTTP host must enforce its header policy,
    /// authorization and body bounds before calling. Conflict leaves intent
    /// unchanged and includes the current Version for a retry response.
    pub fn attempt_if_etag<P>(
        &self,
        expected: &str,
        snapshot: &Snapshot,
        registry: &Registry,
        prepare: P,
    ) -> Result<Outcome, Conflict>
    where
        P: FnOnce(Preparation<'_, R>) -> Result<R, String>,
    {
        let actual = self.status().version();
        if expected != actual.etag() {
            return Err(Conflict { actual });
        }
        self.attempt_if(&actual, snapshot, registry, prepare)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::tests::{invalid_snapshot, prepare_ok, registry, snapshot, store};

    #[test]
    fn tags_track_boot_failed_intent_noop_and_successful_retry() {
        let store = store();
        let registry = registry();
        let boot = store.status().version().etag();
        assert_ne!(boot, GenerationStore::<usize>::new().status().version().etag());
        assert_eq!(boot.len(), 75);
        assert!(matches!(
            store.attempt_if_etag(&boot, &snapshot(1, "same", 8080), &registry, prepare_ok),
            Ok(Outcome::Started { .. })
        ));
        let first = store.status().version().etag();
        assert!(matches!(
            store.attempt_if_etag(&first, &snapshot(2, "same", 8080), &registry, prepare_ok),
            Ok(Outcome::NoOp { .. })
        ));
        let noop = store.status().version().etag();
        assert_ne!(first, noop);
        assert!(matches!(
            store.attempt_if_etag(&noop, &invalid_snapshot(3), &registry, prepare_ok),
            Ok(Outcome::Rejected(_))
        ));
        let failed = store.status().version().etag();
        assert_ne!(noop, failed);
        let retry = snapshot(4, "changed", 8080);
        assert!(matches!(
            store.attempt_if_etag(&failed, &retry, &registry, |_| Err("temporary".into())),
            Ok(Outcome::PrepareFailed(_))
        ));
        let pending = store.status().version().etag();
        assert!(matches!(
            store.attempt_if_etag(&pending, &retry, &registry, prepare_ok),
            Ok(Outcome::Reloaded { .. })
        ));
        assert_ne!(pending, store.status().version().etag());
    }

    #[test]
    fn nonexact_tags_do_not_observe_or_prepare() {
        let store = store();
        let version = store.status().version();
        let tag = version.etag();
        for supplied in [
            String::new(),
            "*".into(),
            format!("W/{tag}"),
            format!("{tag}, {tag}"),
            format!(" {tag}"),
            tag.trim_matches('"').into(),
        ] {
            let conflict = store
                .attempt_if_etag(&supplied, &snapshot(99, "bad", 8080), &registry(), |_| {
                    panic!("must not prepare")
                })
                .unwrap_err();
            assert_eq!(conflict.actual, version);
            assert_eq!(store.status().version(), version);
        }
    }

    #[test]
    fn concurrent_writers_using_one_tag_cannot_both_publish() {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
        let tag = store.status().version().etag();
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let write = |body| {
                let (store, registry, tag, barrier) = (&store, &registry, &tag, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.attempt_if_etag(tag, &snapshot(2, body, 8080), registry, prepare_ok)
                })
            };
            let left = write("left");
            let right = write("right");
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Ok(Outcome::Reloaded { .. })))
                .count(),
            1
        );
        assert_eq!(outcomes.iter().filter(|result| result.is_err()).count(), 1);
        assert_eq!(store.status().generation.number(), 2);
    }
}
