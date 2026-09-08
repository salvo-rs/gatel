use gatel_model::{Generation, Hasher, SourceId, SourceRev, SourceSnapshot, snapshot_codec};

use super::*;
use crate::generation::Outcome;
use crate::generation::tests::{invalid_snapshot, prepare_ok, registry, snapshot, store};

fn consistent(capture: &ConfigurationCapture<usize>) {
    assert_eq!(capture.version().revs, capture.observed_snapshot().revs());
    match capture.committed() {
        None => assert_eq!(capture.version().generation, Generation::NONE),
        Some(committed) => {
            assert_eq!(capture.version().generation, committed.number);
            assert_eq!(capture.version().boot_id, committed.boot_id);
            assert_eq!(committed.source_snapshot().revs(), committed.revs);
        }
    }
}

#[test]
fn captures_initial_failed_noop_and_sealed_states_without_conflating_sources() {
    let store = store();
    let registry = registry();
    let empty = store.capture_configuration();
    consistent(&empty);
    assert!(!empty.is_sealed());
    assert!(empty.observed_snapshot().is_empty());
    store.attempt(&invalid_snapshot(1), &registry, prepare_ok);
    let failed = store.capture_configuration();
    consistent(&failed);
    assert!(failed.committed().is_none());
    assert!(!failed.observed_snapshot().is_empty());

    store.attempt(&snapshot(2, "secret-serving", 8080), &registry, prepare_ok);
    let first = store.capture_configuration();
    consistent(&first);
    assert!(matches!(
        store.attempt(&snapshot(3, "secret-serving", 8080), &registry, prepare_ok),
        Outcome::NoOp { .. }
    ));
    let noop = store.capture_configuration();
    consistent(&noop);
    assert_ne!(first.version(), noop.version());
    assert!(Arc::ptr_eq(first.committed().unwrap(), noop.committed().unwrap()));
    assert_ne!(noop.observed_snapshot().revs(), noop.committed().unwrap().revs);
    store.attempt(&invalid_snapshot(4), &registry, prepare_ok);
    let rejected = store.capture_configuration();
    consistent(&rejected);
    assert!(Arc::ptr_eq(
        first.committed().unwrap(),
        rejected.committed().unwrap()
    ));
    assert!(crate::normalize(rejected.observed_snapshot(), &registry).is_err());
    assert!(crate::normalize(rejected.committed().unwrap().source_snapshot(), &registry).is_ok());
    assert!(!format!("{rejected:?}").contains("secret"));
    store.seal();
    let sealed = store.capture_configuration();
    consistent(&sealed);
    assert!(sealed.is_sealed());
    assert_eq!(sealed.version(), rejected.version());
    assert!(!rejected.is_sealed());
}

#[test]
fn capture_retains_multi_source_tombstones_and_owned_trees_after_store_drop() {
    let store = store();
    let registry = registry();
    let source = SourceId::admin();
    let mut union = snapshot(1, "secret-body", 8080);
    union.insert(SourceSnapshot::new(
        source.clone(),
        SourceRev(1),
        Hasher::new().finish(),
        vec![],
    ));
    store.attempt(&union, &registry, prepare_ok);
    let capture = store.capture_configuration();
    let before = snapshot_codec::encode(capture.observed_snapshot()).unwrap();
    union.insert(invalid_snapshot(2).get(&SourceId::file("main")).unwrap().clone());
    store.attempt(&union, &registry, prepare_ok);
    drop(store);
    consistent(&capture);
    assert_eq!(
        snapshot_codec::encode(capture.observed_snapshot()).unwrap(),
        before
    );
    assert!(capture.observed_snapshot().get(&source).unwrap().nodes.is_empty());
    assert_eq!(capture.observed_snapshot().sources().count(), 2);
    let decoded = snapshot_codec::decode(&before).unwrap();
    assert_eq!(decoded.revs(), capture.version().revs);
    assert!(!format!("{capture:?}").contains("secret-body"));
}

#[test]
fn capture_during_prepare_is_consistent_but_does_not_claim_quiescence() {
    let store = Arc::new(store());
    let registry = registry();
    store.attempt(&snapshot(1, "old", 8080), &registry, prepare_ok);
    let old = store.current().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let worker_store = &store;
        let worker_registry = &registry;
        let worker = scope.spawn(move || {
            worker_store.attempt(&snapshot(2, "new", 8080), worker_registry, |p| {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                prepare_ok(p)
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let pending = store.capture_configuration();
        consistent(&pending);
        assert!(Arc::ptr_eq(&old, pending.committed().unwrap()));
        assert_eq!(
            pending
                .observed_snapshot()
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(2)
        );
        release_tx.send(()).unwrap();
        assert!(matches!(worker.join().unwrap(), Outcome::Reloaded { .. }));
        let after = store.capture_configuration();
        consistent(&after);
        assert_ne!(pending.version().generation, after.version().generation);
        assert_eq!(pending.version().revs, after.version().revs);
        assert!(Arc::ptr_eq(&old, pending.committed().unwrap()));
    });
}

#[test]
fn concurrent_publications_never_tear_capture_version_from_runtime() {
    let store = store();
    let registry = registry();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            barrier.wait();
            for revision in 1..=128 {
                let input = snapshot(revision, &format!("body-{revision}"), 8080);
                assert!(!store.attempt(&input, &registry, prepare_ok).is_failure());
            }
        });
        barrier.wait();
        for _ in 0..1024 {
            consistent(&store.capture_configuration());
            std::thread::yield_now();
        }
        writer.join().unwrap();
    });
    let last = store.capture_configuration();
    consistent(&last);
    assert_eq!(last.version().generation.number(), 128);
}
