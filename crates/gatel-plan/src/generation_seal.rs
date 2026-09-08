use super::tests::*;
use super::*;

#[test]
fn sealing_preserves_readers_and_refuses_all_observation_paths() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
    let current = store.current().unwrap();
    let version = store.status().version();
    store.seal();
    store.seal();
    assert!(store.is_sealed());
    for input in [
        snapshot(1, "first", 8080),
        snapshot(2, "changed", 8080),
        invalid_snapshot(3),
    ] {
        fn never(_: Preparation<'_, usize>) -> Result<usize, String> {
            panic!("sealed store must not prepare");
        }
        assert!(matches!(
            store.attempt(&input, &registry, never),
            Outcome::Stale(_)
        ));
        assert!(matches!(
            store.refresh(&input, &registry, never),
            Outcome::Stale(_)
        ));
        assert!(matches!(
            store.attempt_if(&version, &input, &registry, never),
            Ok(Outcome::Stale(_))
        ));
        assert!(matches!(
            store.refresh_if(&version, &input, &registry, never),
            Ok(Outcome::Stale(_))
        ));
        assert_eq!(store.status().version(), version);
        assert!(store.status().last_failure.is_none());
        assert!(Arc::ptr_eq(&store.current().unwrap(), &current));
    }
}

#[test]
fn sealing_during_prepare_fences_success_and_failure_publication() {
    for fail in [false, true] {
        let store = store();
        let registry = registry();
        store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
        let first = store.current().unwrap();
        let outcome = store.attempt(&snapshot(2, "changed", 8080), &registry, |_| {
            // Preparation runs outside the store lock; sealing here is the
            // deterministic interleaving of shutdown overtaking preparation.
            store.seal();
            if fail { Err("late failure".into()) } else { Ok(42) }
        });
        assert!(matches!(outcome, Outcome::Stale(_)));
        assert!(Arc::ptr_eq(&store.current().unwrap(), &first));
        assert!(store.status().last_failure.is_none());
        assert_eq!(store.status().generation, first.number);
    }
}
