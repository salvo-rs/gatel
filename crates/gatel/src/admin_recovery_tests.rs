use super::*;
use crate::admin::journal::transaction::recovery::{Disposition, Reason};

#[test]
fn checkpoint_summary_distinguishes_noop_empty_intent_without_labeling_it_rejected() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("secret-serving", &registry);
    let store = GenerationStore::<()>::new();
    let mut observed = first.observed_snapshot().clone();
    store.attempt(&observed, &registry, |_| Ok(()));
    observed.insert(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        Hasher::new().finish(),
        vec![],
    ));
    assert!(matches!(
        store.attempt(&observed, &registry, |_| panic!("NoOp")),
        Outcome::NoOp { .. }
    ));
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal
        .save_checkpoint(&registry, &store.capture_configuration())
        .unwrap();
    let inspection = journal.inspect_recovery(&registry).unwrap();
    let (report, code) = crate::admin::journal::cli::report(&inspection);
    assert_eq!(code, 0);
    assert!(report.starts_with("state=checkpoint-current\n"));
    assert!(report.contains("latest_decision=none\n"));
    assert!(report.contains("checkpoint_serving_sources=1\n"));
    assert!(report.contains("checkpoint_observed_sources=2\n"));
    assert!(report.contains("checkpoint_empty_observed_sources=1\n"));
    assert!(report.contains("checkpoint_observed_differs=true\n"));
    assert!(!report.contains("secret"));
}

#[test]
fn inspection_handles_refuse_all_mutation_and_report_checkpoint_states() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("secret-serving-body", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
    drop(journal);
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    let mut journal = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    let current = journal.inspect_recovery(&registry).unwrap();
    let (report, code) = crate::admin::journal::cli::report(&current);
    assert_eq!(code, 0);
    assert!(report.starts_with("state=checkpoint-current\n"));
    assert!(report.contains("checkpoint_serving_sources=1\n"));
    assert!(report.contains("checkpoint_observed_sources=1\n"));
    assert!(report.contains("checkpoint_empty_observed_sources=0\n"));
    assert!(report.contains("checkpoint_observed_differs=false\n"));
    assert!(!report.contains("secret-serving-body"));
    for error in [
        journal.begin("secret-new-body").unwrap_err(),
        journal.discard_incomplete_tail().unwrap_err(),
        journal.save_checkpoint(&registry, &first).unwrap_err(),
    ] {
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    // Rejected writes must not poison the inspection handle.
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::CheckpointCurrent
    );
    drop(journal);
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 3);
    let mut writer = Journal::open(&directory.0, INSTANCE).unwrap();
    let intent = writer.begin("secret-rejected-body").unwrap();
    writer.finish(intent, Decision::Rejected).unwrap();
    drop(writer);
    let mut journal = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    let (report, code) = crate::admin::journal::cli::report(&journal.inspect_recovery(&registry).unwrap());
    assert_eq!(code, 0);
    assert!(report.starts_with("state=checkpoint-with-rejected-intent\n"));
    assert!(!report.contains("secret"));
}

#[test]
fn fresh_checkpoint_and_rejected_intent_remain_distinct() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("serving", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::Uninitialized
    );
    journal.save_checkpoint(&registry, &first).unwrap();
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::CheckpointCurrent
    );
    let intent = journal.begin("rejected-secret;").unwrap();
    journal.finish(intent, Decision::Rejected).unwrap();
    drop(journal);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let before = std::fs::read(directory.0.join("intent.log")).unwrap();
    let inspection = journal.inspect_recovery(&registry).unwrap();
    assert_eq!(inspection.disposition, Disposition::CheckpointWithRejectedIntent);
    assert_eq!(
        inspection.checkpoint.as_ref().unwrap().metadata.plan,
        first.committed().unwrap().plan_hash()
    );
    assert_eq!(inspection.latest.as_ref().unwrap().text, "rejected-secret;");
    assert_eq!(inspection.decision, Some(Decision::Rejected));
    assert_eq!(inspection.journal_sequence, 3);
    assert!(!inspection.incomplete_tail);
    assert!(!format!("{inspection:?}").contains("rejected-secret"));
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), before);
}

#[test]
fn intervening_applied_intent_is_not_hidden_by_a_later_rejection() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let next = committed("next", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
    let applied = journal.begin("").unwrap();
    journal.finish(applied, Decision::Applied).unwrap();
    let rejected = journal.begin("invalid;").unwrap();
    journal.finish(rejected, Decision::Rejected).unwrap();
    drop(journal);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::Reconcile(Reason::AppliedAfterCheckpoint)
    );
    journal.save_checkpoint(&registry, &next).unwrap();
    let current = journal.inspect_recovery(&registry).unwrap();
    assert_eq!(current.applied.as_ref().unwrap().sequence, applied);
    assert_eq!(current.applied.as_ref().unwrap().text, "");
    assert_eq!(current.disposition, Disposition::CheckpointCurrent);
    assert_eq!(current.decision, Some(Decision::Rejected));
    assert_eq!(current.latest.unwrap().text, "invalid;");
    assert_eq!(
        current.checkpoint.unwrap().metadata.plan,
        next.committed().unwrap().plan_hash()
    );
}

#[test]
fn missing_checkpoint_is_not_equivalent_to_a_new_journal() {
    let registry = registry();
    for decision in [Decision::Applied, Decision::Rejected] {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
        let intent = journal.begin("").unwrap();
        journal.finish(intent, decision).unwrap();
        assert_eq!(
            journal.inspect_recovery(&registry).unwrap().disposition,
            Disposition::Reconcile(Reason::MissingCheckpoint)
        );
        assert!(!directory.0.join("checkpoint.bin").exists());
    }
}

#[test]
fn explicit_tail_repair_does_not_clear_unknown_outcome() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
    let intent = journal.begin("unknown;").unwrap();
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::Reconcile(Reason::UnknownOutcome)
    );
    drop(journal);
    let path = directory.0.join("intent.log");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"GAT");
    std::fs::write(&path, &bytes).unwrap();
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let inspection = journal.inspect_recovery(&registry).unwrap();
    assert_eq!(
        inspection.disposition,
        Disposition::Reconcile(Reason::IncompleteTail)
    );
    assert!(inspection.incomplete_tail);
    assert!(inspection.decision.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(journal.discard_incomplete_tail().unwrap(), 3);
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::Reconcile(Reason::UnknownOutcome)
    );
    // Only explicit host reconciliation supplies the result, never inspection.
    journal.finish(intent, Decision::Rejected).unwrap();
    assert_eq!(
        journal.inspect_recovery(&registry).unwrap().disposition,
        Disposition::CheckpointWithRejectedIntent
    );
}

#[test]
fn corrupt_checkpoint_is_an_error_before_any_recovery_classification() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
    let path = directory.0.join("checkpoint.bin");
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(journal.inspect_recovery(&registry).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt");
}
