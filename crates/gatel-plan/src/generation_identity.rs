use gatel_model::{Hasher, SourceId, SourceRev, SourceSnapshot};

use super::tests::*;
use super::*;

fn empty_admin(rev: u64, content: &str) -> SourceSnapshot {
    let mut hash = Hasher::new();
    hash.update_str(content);
    SourceSnapshot::new(SourceId::admin(), SourceRev(rev), hash.finish(), vec![])
}

#[test]
fn moving_a_listener_between_file_sources_publishes_its_new_owner() {
    let store = store();
    let registry = registry();
    let initial = snapshot(1, "same", 8080);
    assert!(matches!(
        store.attempt(&initial, &registry, prepare_ok),
        Outcome::Started { .. }
    ));
    let mut main = initial.sources().next().unwrap().clone();
    let global = main.nodes.remove(0);
    main.rev = SourceRev(2);
    let mut identity = Hasher::new();
    identity.update_str("site only after listener ownership transfer");
    main.content = identity.finish();
    let owner = SourceId::file("listeners");
    let listeners = SourceSnapshot::new(owner.clone(), SourceRev(1), Hasher::new().finish(), vec![global]);
    let changed = gatel_model::Snapshot::new().with(main).with(listeners);
    let outcome = store.attempt(&changed, &registry, prepare_ok);
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    assert_eq!(store.current().unwrap().plan.spec.listeners[0].owner, owner);
}

#[test]
fn source_composition_clones_still_require_whole_generation_cas() {
    let main = SourceId::file("main");
    let store = GenerationStore::<usize>::with_allowed_sources([main.clone(), SourceId::admin()]);
    let registry = registry();
    let inputs = snapshot(1, "base", 8080).with(empty_admin(1, "initial"));
    assert!(matches!(
        store.attempt(&inputs, &registry, prepare_ok),
        Outcome::Started { .. }
    ));
    let version = store.status().version();
    let mut left = inputs.clone();
    let mut right = inputs;
    let admin = empty_admin(2, "new admin intent");
    left.observe_if(admin.source, Some(SourceRev(1)), admin.content, admin.nodes)
        .unwrap();
    let file = snapshot(2, "new file", 8080).get(&main).unwrap().clone();
    right
        .observe_if(main.clone(), Some(SourceRev(1)), file.content, file.nodes.clone())
        .unwrap();
    assert!(matches!(
        store.attempt_if(&version, &left, &registry, prepare_ok),
        Ok(Outcome::NoOp { .. })
    ));
    assert!(
        store
            .attempt_if(&version, &right, &registry, |_| panic!(
                "stale union must not prepare"
            ))
            .is_err()
    );
    assert_eq!(store.status().revs.get(&main), Some(SourceRev(1)));
    assert_eq!(store.status().revs.get(&SourceId::admin()), Some(SourceRev(2)));

    // Recompose against the latest union, rather than discarding the other
    // writer's source or relabelling an old union with a new store version.
    left.observe_if(main.clone(), Some(SourceRev(1)), file.content, file.nodes)
        .unwrap();
    let version = store.status().version();
    assert!(matches!(
        store.attempt_if(&version, &left, &registry, prepare_ok),
        Ok(Outcome::Reloaded { .. })
    ));
    assert_eq!(store.status().revs.get(&main), Some(SourceRev(2)));
    assert_eq!(store.status().revs.get(&SourceId::admin()), Some(SourceRev(2)));
}

#[test]
fn source_registration_checks_exact_identity_before_observing_any_revision() {
    let main = SourceId::file("main");
    let store = GenerationStore::<usize>::with_allowed_sources([main.clone(), SourceId::admin(), main]);
    let registry = registry();
    store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
    let before = store.status().version();
    let mut impostor = empty_admin(1, "unregistered");
    impostor.source = SourceId::file("other");
    let candidate = snapshot(100, "partial overwrite", 8080).with(impostor);
    assert!(matches!(
        store.attempt_if(&before, &candidate, &registry, |_| panic!("unregistered")),
        Ok(Outcome::Stale(_))
    ));
    assert_eq!(store.status().version(), before);
    assert!(matches!(
        store.attempt(
            &snapshot(2, "registered", 8080).with(empty_admin(1, "registered")),
            &registry,
            prepare_ok
        ),
        Outcome::Reloaded { .. }
    ));
    assert_eq!(store.source_limit.get(), 2);
}

#[test]
fn empty_source_registration_denies_all_nonempty_observations() {
    let store = GenerationStore::<usize>::with_allowed_sources([]);
    assert!(matches!(
        store.attempt(&snapshot(1, "denied", 8080), &registry(), |_| panic!("deny all")),
        Outcome::Stale(_)
    ));
    assert!(store.status().revs.is_empty());
}

#[test]
fn source_capacity_is_atomic_and_existing_sources_remain_updatable() {
    let store = GenerationStore::<usize>::with_source_limit(std::num::NonZeroUsize::new(1).unwrap());
    let registry = registry();
    store.attempt(&snapshot(1, "base", 8080), &registry, prepare_ok);
    let before = store.status().version();
    let oversized = snapshot(100, "bad partial advance", 8080).with(empty_admin(1, "new source"));
    let outcome = store.attempt_if(&before, &oversized, &registry, |_| panic!("over capacity"));
    assert!(matches!(outcome, Ok(Outcome::Stale(_))));
    assert_eq!(store.status().version(), before);
    assert!(matches!(
        store.attempt(&snapshot(2, "valid update", 8080), &registry, prepare_ok),
        Outcome::Reloaded { .. }
    ));
}

#[test]
fn failed_intent_and_empty_tombstones_still_consume_source_identity_capacity() {
    let store = GenerationStore::<usize>::with_source_limit(std::num::NonZeroUsize::new(1).unwrap());
    let registry = registry();
    store.attempt(&invalid_snapshot(1), &registry, prepare_ok);
    let failure = store.status().last_failure.unwrap();
    let original = invalid_snapshot(2);
    let mut tombstone = original.sources().next().unwrap().clone();
    tombstone.nodes.clear();
    let only_tombstone = Snapshot::new().with(tombstone);
    // No listener makes this invalid, but its source identity must survive.
    store.attempt(&only_tombstone, &registry, prepare_ok);
    let before = store.status();
    assert!(matches!(
        store.attempt(
            &only_tombstone.with(empty_admin(1, "extra")),
            &registry,
            |_| panic!("over capacity")
        ),
        Outcome::Stale(_)
    ));
    assert_eq!(store.status().version(), before.version());
    assert!(store.status().last_failure.unwrap().at >= failure.at);
    assert_eq!(store.source_limit.get(), 1);
}

#[test]
fn reused_revision_cannot_replace_committed_content() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "original", 8080), &registry, prepare_ok);
    let original = store.current().unwrap();
    let version = store.status().version();
    let outcome = store
        .attempt_if(&version, &snapshot(1, "replacement", 8080), &registry, |_| {
            panic!("changed identity must not prepare")
        })
        .unwrap();
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert_eq!(store.status().version(), version);
    assert!(Arc::ptr_eq(&original, &store.current().unwrap()));
    assert!(!store.status().is_degraded());
}

#[test]
fn rejected_and_noop_observations_also_pin_content_identity() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "original", 8080), &registry, prepare_ok);
    store.attempt(&invalid_snapshot(2), &registry, prepare_ok);
    let failure = store.status().last_failure.unwrap();
    assert!(matches!(
        store.attempt(&snapshot(2, "original", 8080), &registry, prepare_ok),
        Outcome::Stale(_)
    ));
    assert_eq!(store.status().last_failure.unwrap().at, failure.at);
    assert!(matches!(
        store.attempt(&snapshot(3, "original", 8080), &registry, prepare_ok),
        Outcome::NoOp { .. }
    ));
    assert!(matches!(
        store.attempt(&snapshot(3, "changed", 8080), &registry, prepare_ok),
        Outcome::Stale(_)
    ));
}

#[test]
fn conflicting_source_prevents_partial_observation_of_other_sources() {
    let store = store();
    let registry = registry();
    store.attempt(
        &snapshot(1, "original", 8080).with(empty_admin(1, "first")),
        &registry,
        prepare_ok,
    );
    let version = store.status().version();
    let mixed = snapshot(100, "new file", 8080).with(empty_admin(1, "reused revision"));
    assert!(matches!(
        store.attempt(&mixed, &registry, prepare_ok),
        Outcome::Stale(_)
    ));
    assert_eq!(store.status().version(), version);
    let valid = snapshot(2, "new file", 8080).with(empty_admin(1, "first"));
    assert!(matches!(
        store.attempt(&valid, &registry, prepare_ok),
        Outcome::Reloaded { .. }
    ));
}

#[test]
fn unchanged_retry_remains_valid_and_late_content_cannot_poison_head() {
    let store = store();
    let registry = registry();
    let candidate = snapshot(2, "current", 8080);
    assert!(matches!(
        store.attempt(&candidate, &registry, |_| Err("temporary failure".into())),
        Outcome::PrepareFailed(_)
    ));
    assert!(matches!(
        store.attempt(&snapshot(1, "older", 8080), &registry, prepare_ok),
        Outcome::Stale(_)
    ));
    assert!(matches!(
        store.attempt(&candidate, &registry, prepare_ok),
        Outcome::Started { .. }
    ));
}

#[test]
fn equal_revision_replacement_cannot_overtake_inflight_preparation() {
    let store = store();
    let registry = registry();
    let outcome = store.attempt(&snapshot(1, "first observer", 8080), &registry, |cx| {
        assert!(matches!(
            store.attempt(&snapshot(1, "racing replacement", 8080), &registry, |_| panic!(
                "not prepared"
            )),
            Outcome::Stale(_)
        ));
        prepare_ok(cx)
    });
    assert!(matches!(outcome, Outcome::Started { .. }));
}
