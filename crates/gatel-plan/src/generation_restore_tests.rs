use gatel_model::{Hasher, SourceId, SourceRev, SourceSnapshot, snapshot_codec};

use super::*;
use crate::generation::Outcome;
use crate::generation::tests::{invalid_snapshot, prepare_ok, registry, snapshot, store};

#[test]
fn publication_between_restore_validation_and_commit_wins_without_losing_sources() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "serving", 8080), &registry, prepare_ok);
    let expected = store.status().version();
    let candidate = invalid_snapshot(2);
    let winner = snapshot(3, "winner", 8080);
    let result = store.restore_intent_with(&expected, &candidate, || {
        assert!(matches!(
            store.attempt(&winner, &registry, prepare_ok),
            Outcome::Reloaded { .. }
        ));
    });
    let Err(RestoreIntentError::Conflict(conflict)) = result else {
        panic!("late restore must conflict")
    };
    assert_eq!(conflict.actual, store.status().version());
    assert_eq!(
        snapshot_codec::encode(&store.observed_snapshot().1).unwrap(),
        snapshot_codec::encode(&winner).unwrap()
    );
    assert_eq!(store.current().unwrap().revs, winner.revs());
    assert!(store.status().last_failure.is_none());
}

#[test]
fn same_revision_refresh_and_shutdown_are_rechecked_at_restore_commit() {
    for seal in [false, true] {
        let store = store();
        let registry = registry();
        let initial = snapshot(1, "serving", 8080);
        store.attempt(&initial, &registry, prepare_ok);
        let expected = store.status().version();
        let result = store.restore_intent_with(&expected, &invalid_snapshot(2), || {
            if seal {
                store.seal();
            } else {
                assert!(matches!(
                    store.refresh(&initial, &registry, |_| Ok(99)),
                    Outcome::Reloaded { .. }
                ));
            }
        });
        if seal {
            assert!(matches!(result, Err(RestoreIntentError::Sealed)));
            assert_eq!(store.status().version(), expected);
        } else {
            assert!(matches!(result, Err(RestoreIntentError::Conflict(_))));
            assert_eq!(store.current().unwrap().runtime, 99);
            assert_eq!(store.status().version().revs, expected.revs);
            assert_ne!(store.status().version().generation, expected.generation);
        }
        assert_eq!(
            snapshot_codec::encode(&store.observed_snapshot().1).unwrap(),
            snapshot_codec::encode(&initial).unwrap()
        );
    }
}

#[test]
fn restoring_rejected_intent_keeps_published_runtime_and_new_boot() {
    let old = store();
    let registry = registry();
    old.attempt(&snapshot(1, "serving", 8080), &registry, prepare_ok);
    old.attempt(&invalid_snapshot(2), &registry, prepare_ok);
    let persisted = old.capture_configuration();
    let restored = store();
    restored.attempt(
        persisted.committed().unwrap().source_snapshot(),
        &registry,
        prepare_ok,
    );
    let serving = restored.current().unwrap();
    let expected = restored.status().version();
    let version = restored
        .restore_intent_if(&expected, persisted.observed_snapshot())
        .unwrap();
    assert_ne!(version.boot_id, persisted.version().boot_id);
    assert_eq!(version.generation, expected.generation);
    assert_eq!(version.revs, persisted.version().revs);
    assert!(Arc::ptr_eq(&serving, &restored.current().unwrap()));
    assert!(crate::normalize(&restored.observed_snapshot().1, &registry).is_err());
    assert!(
        restored
            .restore_intent_if(&expected, persisted.observed_snapshot())
            .is_err()
    );
    assert_eq!(
        restored
            .restore_intent_if(&version, persisted.observed_snapshot())
            .unwrap(),
        version
    );
    // Correcting a different source must not forget recovered rejected intent.
    let admin = SourceSnapshot::new(SourceId::admin(), SourceRev(1), Hasher::new().finish(), vec![]);
    assert!(matches!(
        restored.attempt_source_if(&version, admin, &registry, prepare_ok),
        Ok(Outcome::Rejected(_))
    ));
    assert!(Arc::ptr_eq(&serving, &restored.current().unwrap()));
}

#[test]
fn failed_mixed_restoration_does_not_partially_advance_intent() {
    let store = store();
    let registry = registry();
    let initial = snapshot(3, "secret-serving", 8080);
    store.attempt(&initial, &registry, prepare_ok);
    let expected = store.status().version();
    let before = snapshot_codec::encode(&store.observed_snapshot().1).unwrap();
    let mut invalid = vec![
        Snapshot::new(),
        snapshot(2, "older", 8080),
        snapshot(3, "changed", 8080),
    ];
    let mut same_identity = initial.sources().next().unwrap().clone();
    same_identity.nodes.clear();
    invalid.push(Snapshot::new().with(same_identity));
    invalid.push(snapshot(4, "new", 8080).with(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(0),
        Hasher::new().finish(),
        vec![],
    )));
    for input in invalid {
        assert!(matches!(
            store.restore_intent_if(&expected, &input),
            Err(RestoreIntentError::InvalidSources)
        ));
        assert_eq!(store.status().version(), expected);
        assert_eq!(
            snapshot_codec::encode(&store.observed_snapshot().1).unwrap(),
            before
        );
    }
}

#[test]
fn restoration_preserves_tombstones_and_fences_preparing_candidate() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "serving", 8080), &registry, prepare_ok);
    let serving = store.current().unwrap();
    let tombstone = SourceSnapshot::new(SourceId::admin(), SourceRev(7), Hasher::new().finish(), vec![]);
    let recovered = invalid_snapshot(3).with(tombstone);
    let outcome = store.attempt(&snapshot(2, "candidate", 8080), &registry, |_| {
        store
            .restore_intent_if(&store.status().version(), &recovered)
            .unwrap();
        Ok(99)
    });
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert!(Arc::ptr_eq(&serving, &store.current().unwrap()));
    let captured = store.capture_configuration();
    assert_eq!(
        captured.observed_snapshot().get(&SourceId::admin()).unwrap().rev,
        SourceRev(7)
    );
    assert!(
        captured
            .observed_snapshot()
            .get(&SourceId::admin())
            .unwrap()
            .nodes
            .is_empty()
    );
}

#[test]
fn restoration_obeys_capacity_source_allowlist_boot_and_seal() {
    let registry = registry();
    let store = GenerationStore::<usize>::with_source_limit(std::num::NonZeroUsize::new(1).unwrap());
    let initial = snapshot(1, "serving", 8080);
    store.attempt(&initial, &registry, prepare_ok);
    let extra = initial.clone().with(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        Hasher::new().finish(),
        vec![],
    ));
    let expected = store.status().version();
    assert!(matches!(
        store.restore_intent_if(&expected, &extra),
        Err(RestoreIntentError::InvalidSources)
    ));
    let foreign = GenerationStore::<usize>::new().status().version();
    assert!(matches!(
        store.restore_intent_if(&foreign, &initial),
        Err(RestoreIntentError::Conflict(_))
    ));
    store.seal();
    assert!(matches!(
        store.restore_intent_if(&expected, &initial),
        Err(RestoreIntentError::Sealed)
    ));
    assert_eq!(store.status().version(), expected);
    let restricted =
        GenerationStore::<usize>::with_allowed_sources([SourceId::file("main"), SourceId::file("other")]);
    restricted.attempt(&initial, &registry, prepare_ok);
    assert!(matches!(
        restricted.restore_intent_if(&restricted.status().version(), &extra),
        Err(RestoreIntentError::InvalidSources)
    ));
}
