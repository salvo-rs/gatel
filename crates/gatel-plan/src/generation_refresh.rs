use super::tests::*;
use super::*;

#[test]
fn refresh_rebuilds_an_identical_plan_without_changing_source_revisions() {
    let store = store();
    let registry = registry();
    let input = snapshot(1, "same", 8080);
    store.attempt(&input, &registry, prepare_ok);
    let before = store.current().unwrap();
    assert!(matches!(
        store.attempt(&input, &registry, |_| panic!("normal NoOp")),
        Outcome::NoOp { .. }
    ));
    let outcome = store.refresh(&input, &registry, |preparation| {
        assert_eq!(preparation.previous.unwrap().runtime, before.runtime);
        Ok(77)
    });
    assert!(matches!(outcome, Outcome::Reloaded { .. }));
    let after = store.current().unwrap();
    assert_eq!(after.runtime, 77);
    assert_eq!(after.plan_hash(), before.plan_hash());
    assert_eq!(store.status().revs, input.revs());
    assert_eq!(after.number.number(), before.number.number() + 1);
}

#[test]
fn invalid_resource_refresh_keeps_the_serving_generation() {
    let store = store();
    let registry = registry();
    let input = snapshot(1, "same", 8080);
    store.attempt(&input, &registry, prepare_ok);
    let before = store.current().unwrap();
    assert!(matches!(
        store.refresh(&input, &registry, |_| Err("resource unavailable".into())),
        Outcome::PrepareFailed(_)
    ));
    assert!(Arc::ptr_eq(&before, &store.current().unwrap()));
    assert!(store.status().last_failure.is_some());
    assert!(matches!(
        store.refresh(&input, &registry, prepare_ok),
        Outcome::Reloaded { .. }
    ));
    assert!(store.status().last_failure.is_none());
}

#[test]
fn a_refresh_overtaken_by_another_refresh_cannot_publish() {
    let store = store();
    let registry = registry();
    let input = snapshot(1, "same", 8080);
    store.attempt(&input, &registry, prepare_ok);
    let outcome = store.refresh(&input, &registry, |_| {
        assert!(matches!(
            store.refresh(&input, &registry, |_| Ok(23)),
            Outcome::Reloaded { .. }
        ));
        Ok(99)
    });
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert_eq!(store.current().unwrap().runtime, 23);
}

#[test]
fn conditional_refresh_fences_the_generation_even_with_identical_revisions() {
    let store = store();
    let registry = registry();
    let input = snapshot(1, "same", 8080);
    store.attempt(&input, &registry, prepare_ok);
    let version = store.status().version();
    assert!(matches!(
        store.refresh_if(&version, &input, &registry, prepare_ok),
        Ok(Outcome::Reloaded { .. })
    ));
    assert!(
        store
            .refresh_if(&version, &input, &registry, |_| panic!("conflicting refresh"))
            .is_err()
    );
}

#[test]
fn refresh_does_not_bypass_source_identity_validation() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(2, "current", 8080), &registry, prepare_ok);
    for input in [snapshot(1, "older", 8080), snapshot(2, "relabelled", 8080)] {
        assert!(matches!(
            store.refresh(&input, &registry, |_| panic!("stale refresh")),
            Outcome::Stale(_)
        ));
    }
}
