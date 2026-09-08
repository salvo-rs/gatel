use super::*;
use crate::generation::Outcome;
use crate::generation::tests::{invalid_snapshot, prepare_ok, registry, snapshot, store};

#[test]
fn bootstrap_publishes_serving_with_rejected_intent_already_in_place() {
    let store = store();
    let registry = registry();
    let serving = snapshot(1, "serving-secret", 8080);
    let observed = invalid_snapshot(3);
    let expected = store.status().version();
    let result = store
        .recover_if(&expected, &serving, &observed, &registry, |p| {
            assert!(p.previous.is_none());
            assert!(store.current().is_none());
            assert_eq!(store.observed_snapshot().1.revs(), observed.revs());
            prepare_ok(p)
        })
        .unwrap();
    assert!(matches!(result, Outcome::Started { .. }));
    assert!(store.status().observed_differs);
    let capture = store.capture_configuration();
    assert_eq!(capture.version().boot_id, expected.boot_id);
    assert_eq!(capture.version().revs, observed.revs());
    assert_eq!(capture.committed().unwrap().revs, serving.revs());
    assert!(crate::normalize(capture.observed_snapshot(), &registry).is_err());
    assert!(crate::normalize(capture.committed().unwrap().source_snapshot(), &registry).is_ok());
}

#[test]
fn invalid_pair_does_not_observe_or_prepare_anything() {
    let store = store();
    let registry = registry();
    let expected = store.status().version();
    let serving = snapshot(2, "serving", 8080);
    for observed in [
        Snapshot::new(),
        snapshot(1, "old", 8080),
        snapshot(2, "changed", 8080),
    ] {
        assert!(matches!(
            store.recover_if(&expected, &serving, &observed, &registry, |_| panic!(
                "invalid pair reached prepare"
            )),
            Err(RestoreIntentError::InvalidSources)
        ));
        assert_eq!(store.status().version(), expected);
        assert!(store.current().is_none());
        assert!(store.observed_snapshot().1.is_empty());
    }
}

#[test]
fn preparation_failure_is_retryable_without_applying_latest_intent() {
    let store = store();
    let registry = registry();
    let serving = snapshot(1, "serving", 8080);
    let observed = invalid_snapshot(2);
    assert!(matches!(
        store.recover_if(
            &store.status().version(),
            &serving,
            &observed,
            &registry,
            |_| Err("resource unavailable".into())
        ),
        Ok(Outcome::PrepareFailed(_))
    ));
    assert!(store.current().is_none());
    assert_eq!(store.status().version().revs, observed.revs());
    assert!(store.status().last_failure.is_some());
    assert!(matches!(
        store.recover_if(
            &store.status().version(),
            &serving,
            &observed,
            &registry,
            prepare_ok
        ),
        Ok(Outcome::Started { .. })
    ));
    assert!(store.status().last_failure.is_none());
}

#[test]
fn running_host_stale_boot_and_sealed_store_cannot_bootstrap() {
    let store = store();
    let registry = registry();
    let serving = snapshot(1, "serving", 8080);
    let foreign = GenerationStore::<usize>::new().status().version();
    assert!(matches!(
        store.recover_if(&foreign, &serving, &serving, &registry, |_| panic!(
            "foreign boot"
        )),
        Err(RestoreIntentError::Conflict(_))
    ));
    store.attempt(&serving, &registry, prepare_ok);
    assert!(matches!(
        store.recover_if(
            &store.status().version(),
            &serving,
            &serving,
            &registry,
            |_| panic!("live rollback")
        ),
        Err(RestoreIntentError::AlreadyPublished)
    ));
    let cold = GenerationStore::<usize>::new();
    cold.seal();
    assert!(matches!(
        cold.recover_if(
            &cold.status().version(),
            &serving,
            &serving,
            &registry,
            |_| panic!("sealed")
        ),
        Err(RestoreIntentError::Sealed)
    ));
}

#[test]
fn publication_or_shutdown_during_recovery_prepare_prevents_late_commit() {
    for seal in [false, true] {
        let store = store();
        let registry = registry();
        let serving = snapshot(1, "old", 8080);
        let observed = invalid_snapshot(2);
        let result = store
            .recover_if(&store.status().version(), &serving, &observed, &registry, |_| {
                if seal {
                    store.seal();
                } else {
                    assert!(matches!(
                        store.attempt(&snapshot(3, "winner", 8080), &registry, |_| Ok(99)),
                        Outcome::Started { .. }
                    ));
                }
                Ok(7)
            })
            .unwrap();
        assert!(matches!(result, Outcome::Stale(_)));
        if seal {
            assert!(store.current().is_none());
        } else {
            assert_eq!(store.current().unwrap().runtime, 99);
        }
    }
}
