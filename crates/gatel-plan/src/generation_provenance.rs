use gatel_model::{SourceId, SourceRev, SourceSnapshot};

use super::tests::*;
use super::*;

#[test]
fn committed_input_survives_rejection_prepare_failure_and_caller_mutation() {
    let store = store();
    let registry = registry();
    let mut original = snapshot(1, "private-first", 8080);
    store.attempt(&original, &registry, prepare_ok);
    let pinned = store.current().unwrap();
    let source = SourceId::file("main");
    let content = original.get(&source).unwrap().content;
    original.insert(invalid_snapshot(2).get(&source).unwrap().clone());
    assert!(matches!(
        store.attempt(&original, &registry, prepare_ok),
        Outcome::Rejected(_)
    ));
    assert!(matches!(
        store.attempt(&snapshot(3, "failed", 8080), &registry, |_| Err("fixture".into())),
        Outcome::PrepareFailed(_)
    ));
    assert_eq!(
        store.observed_snapshot().1.get(&source).unwrap().rev,
        SourceRev(3)
    );
    assert_eq!(pinned.source_snapshot().get(&source).unwrap().rev, SourceRev(1));
    assert_eq!(pinned.source_snapshot().get(&source).unwrap().content, content);
    assert_eq!(pinned.source_snapshot().revs(), pinned.revs);
    assert!(!format!("{pinned:?}").contains("private-first"));
    assert!(Arc::ptr_eq(&pinned, &store.current().unwrap()));
    store.attempt(&snapshot(4, "replacement", 8080), &registry, prepare_ok);
    assert_eq!(
        store
            .current()
            .unwrap()
            .source_snapshot()
            .get(&source)
            .unwrap()
            .rev,
        SourceRev(4)
    );
    assert_eq!(pinned.source_snapshot().get(&source).unwrap().content, content);
}

#[test]
fn noop_keeps_original_build_input_but_refresh_captures_new_revisions() {
    let store = store();
    let registry = registry();
    assert!(!store.status().observed_differs);
    store.attempt(&snapshot(1, "same", 8080), &registry, prepare_ok);
    assert!(!store.status().observed_differs);
    let pinned = store.current().unwrap();
    let equivalent = snapshot(2, "same", 8080);
    assert!(matches!(
        store.attempt(&equivalent, &registry, |_| panic!("NoOp prepares")),
        Outcome::NoOp { .. }
    ));
    assert!(Arc::ptr_eq(&pinned, &store.current().unwrap()));
    assert_ne!(
        pinned.source_snapshot().revs(),
        store.observed_snapshot().1.revs()
    );
    assert!(store.status().observed_differs);
    assert!(!store.status().is_degraded(), "NoOp divergence is not failure");
    assert!(matches!(
        store.refresh(&equivalent, &registry, prepare_ok),
        Outcome::Reloaded { .. }
    ));
    assert_eq!(
        store.current().unwrap().source_snapshot().revs(),
        equivalent.revs()
    );
    assert!(!store.status().observed_differs);
    assert_eq!(pinned.source_snapshot().revs(), snapshot(1, "same", 8080).revs());
}

#[test]
fn superseded_candidate_cannot_replace_winner_provenance() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
    let winner = snapshot(3, "winner", 8080);
    let result = store.attempt(&snapshot(2, "loser", 8080), &registry, |_| {
        assert!(matches!(
            store.attempt(&winner, &registry, prepare_ok),
            Outcome::Reloaded { .. }
        ));
        Ok(99)
    });
    assert!(matches!(result, Outcome::Stale(_)));
    let current = store.current().unwrap();
    let main = SourceId::file("main");
    assert_eq!(
        current.source_snapshot().get(&main).unwrap().content,
        winner.get(&main).unwrap().content
    );
    assert_eq!(current.source_snapshot().revs(), current.revs);
    assert_ne!(current.runtime, 99);
}

#[test]
fn full_source_union_and_empty_contributions_are_pinned_together() {
    let store = store();
    let registry = registry();
    let admin = SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        gatel_model::Hasher::new().finish(),
        vec![],
    );
    let union = snapshot(1, "first", 8080).with(admin.clone());
    store.attempt(&union, &registry, prepare_ok);
    let pinned = store.current().unwrap();
    assert_eq!(pinned.source_snapshot().sources().count(), 2);
    assert!(
        pinned
            .source_snapshot()
            .get(&SourceId::admin())
            .unwrap()
            .nodes
            .is_empty()
    );
    let next = snapshot(2, "next", 8080).with(admin);
    assert!(matches!(
        store.attempt(&next, &registry, prepare_ok),
        Outcome::Reloaded { .. }
    ));
    assert_eq!(store.current().unwrap().source_snapshot().revs(), next.revs());
    assert_eq!(pinned.source_snapshot().revs(), union.revs());
    // Recompiling a retained input is independent of latest failed intent.
    let encoded = gatel_model::snapshot_codec::encode(pinned.source_snapshot()).unwrap();
    let decoded = gatel_model::snapshot_codec::decode(&encoded).unwrap();
    let restored = super::tests::store();
    assert!(matches!(
        restored.attempt(&decoded, &registry, prepare_ok),
        Outcome::Started { .. }
    ));
    assert_eq!(restored.current().unwrap().plan_hash(), pinned.plan_hash());
    assert_ne!(restored.current().unwrap().boot_id, pinned.boot_id);
}
