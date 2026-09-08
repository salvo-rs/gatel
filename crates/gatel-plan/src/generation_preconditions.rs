use super::tests::*;
use super::*;

#[test]
fn conflict_does_not_observe_prepare_or_clear_failure() {
    let store = store();
    let registry = registry();
    let version = store.status().version();
    store.attempt(&invalid_snapshot(1), &registry, prepare_ok);
    let before = store.status();
    let conflict = store
        .attempt_if(&version, &snapshot(100, "bad overwrite", 8080), &registry, |_| {
            panic!("conflicting candidate must not prepare")
        })
        .unwrap_err();
    assert_eq!(conflict.actual, before.version());
    let after = store.status();
    assert_eq!(after.version(), before.version());
    assert_eq!(after.last_failure.unwrap().at, before.last_failure.unwrap().at);
    assert!(matches!(
        store.attempt_if(
            &conflict.actual,
            &snapshot(2, "recovery", 8080),
            &registry,
            prepare_ok
        ),
        Ok(Outcome::Started { .. })
    ));
}

#[test]
fn no_op_intent_invalidates_version_without_consuming_generation() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "same", 8080), &registry, prepare_ok);
    let version = store.status().version();
    assert!(matches!(
        store.attempt_if(&version, &snapshot(2, "same", 8080), &registry, |_| panic!(
            "no-op"
        )),
        Ok(Outcome::NoOp { .. })
    ));
    assert_eq!(store.status().generation, version.generation);
    assert!(
        store
            .attempt_if(&version, &snapshot(3, "overwrite", 8080), &registry, prepare_ok)
            .is_err()
    );
}

#[test]
fn versions_are_not_reusable_across_store_lifetimes() {
    let first = store();
    let second = store();
    assert!(
        second
            .attempt_if(
                &first.status().version(),
                &snapshot(1, "new", 8080),
                &registry(),
                prepare_ok,
            )
            .is_err()
    );
    assert!(!second.status().is_serving());
}

#[test]
fn successful_retry_invalidates_generation_even_when_head_is_unchanged() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
    let candidate = snapshot(2, "retry", 8080);
    assert!(matches!(
        store.attempt(&candidate, &registry, |_| Err("temporary bind failure".into())),
        Outcome::PrepareFailed(_)
    ));
    let version = store.status().version();
    assert!(matches!(
        store.attempt_if(&version, &candidate, &registry, prepare_ok),
        Ok(Outcome::Reloaded { .. })
    ));
    assert_eq!(store.status().revs, version.revs);
    assert!(
        store
            .attempt_if(&version, &candidate, &registry, prepare_ok)
            .is_err()
    );
}

#[test]
fn two_writers_from_one_version_cannot_both_observe_new_intent() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
    let version = store.status().version();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let start = |body| {
            let version = &version;
            let barrier = &barrier;
            let store = &store;
            let registry = &registry;
            scope.spawn(move || {
                barrier.wait();
                store.attempt_if(version, &snapshot(2, body, 8080), registry, prepare_ok)
            })
        };
        let left = start("left");
        let right = start("right");
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Ok(Outcome::Reloaded { .. })))
            .count(),
        1
    );
    assert_eq!(outcomes.iter().filter(|o| o.is_err()).count(), 1);
    assert_eq!(store.status().generation.number(), 2);
}

#[test]
fn admitted_conditional_write_still_loses_to_newer_observation() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
    let version = store.status().version();
    let result = store.attempt_if(&version, &snapshot(2, "slow", 8080), &registry, |cx| {
        assert!(matches!(
            store.attempt(&snapshot(3, "newer", 8080), &registry, prepare_ok),
            Outcome::Reloaded { .. }
        ));
        prepare_ok(cx)
    });
    assert!(matches!(result, Ok(Outcome::Stale(_))));
    assert_eq!(store.current().unwrap().revs, snapshot(3, "newer", 8080).revs());
}
