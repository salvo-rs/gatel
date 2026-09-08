use gatel_model::snapshot_codec;

use super::*;

#[test]
fn interrupted_compaction_exposes_no_partial_destination_and_preserves_source() {
    use super::super::compact::{Phase, publish_with};
    for stop in [
        Phase::LogSynced,
        Phase::CheckpointSynced,
        Phase::BeforePublish,
        Phase::Published,
    ] {
        let directory = Directory::new();
        let registry = registry();
        let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
        journal
            .save_checkpoint(&registry, &committed("source", &registry))
            .unwrap();
        let source_log = std::fs::read(directory.0.join("intent.log")).unwrap();
        let source_checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
        let (checkpoint, log) =
            checkpoint::compact(&source_checkpoint, INSTANCE, &registry, &source_log).unwrap();
        let destination = directory.0.join("compacted");
        assert!(
            publish_with(&destination, &checkpoint, &log, |phase| {
                if phase == stop {
                    Err(io::Error::other("injected compaction failure"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(destination.exists(), stop == Phase::Published);
        if destination.exists() {
            let mut restored = Journal::open_existing(&destination, INSTANCE).unwrap();
            assert!(restored.load_checkpoint(&registry).unwrap().is_some());
        }
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), source_log);
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            source_checkpoint
        );
        assert!(!std::fs::read_dir(&directory.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gatel-compact-")
        }));
    }
}

#[test]
fn compaction_preserves_dual_snapshots_and_publication_but_reclaims_log_capacity() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("serving-secret", &registry);
    let store = GenerationStore::<()>::new();
    let mut observed = first.committed().unwrap().source_snapshot().clone();
    store.attempt(&observed, &registry, |_| Ok(()));
    observed.insert(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        Hasher::new().finish(),
        gatel_kdl::lower("unknown-directive \"rejected-secret\";", "admin").unwrap(),
    ));
    assert!(matches!(
        store.attempt(&observed, &registry, |_| Ok(())),
        Outcome::Rejected(_)
    ));
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    for _ in 0..8 {
        let intent = journal.begin("historical-secret;").unwrap();
        journal.finish(intent, Decision::Rejected).unwrap();
    }
    journal
        .save_checkpoint(&registry, &store.capture_configuration())
        .unwrap();
    let before = journal.load_checkpoint(&registry).unwrap().unwrap();
    drop(journal);
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    let destination = directory.0.join("compacted");
    let mut source = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    assert!(Journal::open_existing(&directory.0, INSTANCE).is_err());
    source.compact_to(&registry, &destination).unwrap();
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert!(std::fs::metadata(destination.join("intent.log")).unwrap().len() < log.len() as u64);
    let mut compacted = Journal::open_existing(&destination, INSTANCE).unwrap();
    let inspection = compacted.inspect_recovery(&registry).unwrap();
    assert_eq!(
        inspection.disposition,
        crate::admin::journal::transaction::recovery::Disposition::CheckpointCurrent
    );
    let after = inspection.checkpoint.unwrap();
    assert_eq!(after.metadata.journal_sequence, 1);
    assert_eq!(after.metadata.publication, before.metadata.publication);
    assert_eq!(after.metadata.plan, before.metadata.plan);
    assert_eq!(
        snapshot_codec::encode(&after.sources).unwrap(),
        snapshot_codec::encode(&before.sources).unwrap()
    );
    assert_eq!(
        snapshot_codec::encode(&after.observed).unwrap(),
        snapshot_codec::encode(&before.observed).unwrap()
    );
    assert!(gatel_plan::normalize(&after.observed, &registry).is_err());
    assert!(
        compacted.state().latest.is_none(),
        "history intentionally compacted"
    );
    drop(compacted);
    let mut writable = Journal::open(&destination, INSTANCE).unwrap();
    let intent = writable.begin("next;").unwrap();
    assert_eq!(intent, 2);
    writable.finish(intent, Decision::Rejected).unwrap();
    let preserved = std::fs::read(destination.join("intent.log")).unwrap();
    assert!(source.compact_to(&registry, &destination).is_err());
    assert_eq!(std::fs::read(destination.join("intent.log")).unwrap(), preserved);
}

#[test]
fn compaction_refuses_uncovered_pending_applied_rejected_and_incomplete_history() {
    for decision in [None, Some(Decision::Applied), Some(Decision::Rejected)] {
        let directory = Directory::new();
        let registry = registry();
        let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
        journal
            .save_checkpoint(&registry, &committed("old", &registry))
            .unwrap();
        let intent = journal.begin("uncovered;").unwrap();
        if let Some(decision) = decision {
            journal.finish(intent, decision).unwrap();
        }
        let destination = directory.0.join("compacted");
        let log = std::fs::read(directory.0.join("intent.log")).unwrap();
        assert!(journal.compact_to(&registry, &destination).is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    }
    let directory = Directory::new();
    let registry = registry();
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let destination = directory.0.join("compacted");
    assert!(
        journal.compact_to(&registry, &destination).is_err(),
        "missing checkpoint"
    );
    journal
        .save_checkpoint(&registry, &committed("old", &registry))
        .unwrap();
    drop(journal);
    let log_path = directory.0.join("intent.log");
    OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap()
        .write_all(b"GATE")
        .unwrap();
    let mut journal = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    assert!(journal.compact_to(&registry, &destination).is_err());
    assert!(!destination.exists());
}
