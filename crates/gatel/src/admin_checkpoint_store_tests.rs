use gatel_model::{Hasher, Snapshot, SourceId, SourceRev, SourceSnapshot};
use gatel_plan::{GenerationStore, Outcome};

use super::*;
use crate::admin::journal::binding;
use crate::admin::journal::transaction::{Decision, Journal, State};

#[path = "admin_checkpoint_crash_tests.rs"]
mod crash;

#[path = "admin_recovery_tests.rs"]
mod recovery;

#[path = "admin_bootstrap_tests.rs"]
mod bootstrap;

#[path = "admin_compact_tests.rs"]
mod compaction;

#[path = "admin_compact_crash_tests.rs"]
mod compaction_crash;

const INSTANCE: &str = "checkpoint-disk-fixture";

#[test]
fn identical_checkpoint_skips_replacement_but_still_validates_existing_material() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("unchanged", &registry);
    let mut store = open(&directory);
    store.save_checkpoint(INSTANCE, &registry, &first).unwrap();
    let path = directory.0.join("checkpoint.bin");
    let before = std::fs::read(&path).unwrap();
    store
        .save_checkpoint_with(INSTANCE, &registry, &first, |_| {
            panic!("identical envelope must not stage, sync or rename")
        })
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!store.poisoned);
    // Same plan in a different host lifetime still has a different envelope.
    let next = committed("unchanged", &registry);
    assert_eq!(
        first.committed().unwrap().plan_hash(),
        next.committed().unwrap().plan_hash()
    );
    let mut phases = Vec::new();
    store
        .save_checkpoint_with(INSTANCE, &registry, &next, |phase| {
            phases.push(phase);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        phases,
        [
            Phase::Written,
            Phase::Synced,
            Phase::BeforeRename,
            Phase::Renamed,
            Phase::DirectorySynced
        ]
    );
    let mut corrupt = std::fs::read(&path).unwrap();
    corrupt[8] ^= 1;
    std::fs::write(&path, &corrupt).unwrap();
    assert!(
        store
            .save_checkpoint_with(INSTANCE, &registry, &next, |_| {
                panic!("corruption must be rejected before staging")
            })
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gatel-checkpoint-{}", gatel_model::BootId::generate()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn registry() -> Registry {
    Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .unwrap()
}
fn committed(body: &str, registry: &Registry) -> ConfigurationCapture<()> {
    let text = format!(
        "global {{ http \"127.0.0.1:8080\"; }}\nsite \"*\" {{\nroute \"/\" {{ respond \"{body}\"; }}\n}}"
    );
    let mut hash = Hasher::new();
    hash.update_str(&text);
    let source = SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        hash.finish(),
        gatel_kdl::lower(&text, "fixture").unwrap(),
    );
    let store = GenerationStore::<()>::new();
    assert!(matches!(
        store.attempt(&Snapshot::new().with(source), registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    store.capture_configuration()
}
fn open(directory: &Directory) -> Store {
    let mut state = State::default();
    binding::open(&directory.0, INSTANCE, |seq, text| state.accept(seq, text)).unwrap()
}

#[test]
fn host_checkpoint_writer_refuses_uncovered_intents_without_rewriting_files() {
    use crate::admin::CheckpointWriter;
    for decision in [None, Some(Decision::Applied), Some(Decision::Rejected)] {
        let directory = Directory::new();
        let registry = registry();
        let capture = committed("covered", &registry);
        let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
        journal.save_checkpoint(&registry, &capture).unwrap();
        let intent = journal.begin("uncovered-secret;").unwrap();
        if let Some(decision) = decision {
            journal.finish(intent, decision).unwrap();
        }
        drop(journal);
        let log = std::fs::read(directory.0.join("intent.log")).unwrap();
        let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
        assert!(CheckpointWriter::open(&directory.0, INSTANCE, &registry).is_err());
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            checkpoint
        );
        // Failure must release the writer lock so explicit inspection can proceed.
        assert!(Journal::open_existing(&directory.0, INSTANCE).is_ok());
    }
}

#[test]
fn host_checkpoint_failure_stops_future_saves_even_before_disk_mutation() {
    use crate::admin::CheckpointWriter;
    let directory = Directory::new();
    let registry = registry();
    let generation = GenerationStore::<()>::new();
    let invalid = generation.capture_configuration();
    let seed = committed("original", &registry);
    generation.attempt(seed.committed().unwrap().source_snapshot(), &registry, |_| Ok(()));
    let original = generation.capture_configuration();
    let replacement = committed("replacement", &registry);
    let (mut writer, snapshots) = CheckpointWriter::open(&directory.0, INSTANCE, &registry).unwrap();
    assert!(snapshots.is_none());
    writer.save(&registry, &original).unwrap();
    writer.ensure_ready().unwrap();
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    // Encoding can fail after host publication, for example due to snapshot
    // capacity or provenance. No committed generation is a deterministic case.
    assert!(writer.save(&registry, &invalid).is_err());
    assert!(writer.ensure_ready().is_err());
    assert!(writer.save(&registry, &replacement).is_err());
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert!(CheckpointWriter::open(&directory.0, INSTANCE, &registry).is_err());
    drop(writer);
    let (writer, snapshots) = CheckpointWriter::open(&directory.0, INSTANCE, &registry).unwrap();
    assert!(writer.ensure_ready().is_err(), "reopening alone is not recovery");
    let (serving, _) = snapshots.unwrap();
    assert_eq!(
        gatel_model::snapshot_codec::encode(&serving).unwrap(),
        gatel_model::snapshot_codec::encode(original.committed().unwrap().source_snapshot()).unwrap()
    );
}

#[test]
fn reopened_writer_requires_exact_serving_and_observed_recovery() {
    use crate::admin::CheckpointWriter;
    let directory = Directory::new();
    let registry = registry();
    let seed = committed("serving", &registry);
    let generation = GenerationStore::<()>::new();
    let serving = seed.committed().unwrap().source_snapshot().clone();
    generation.attempt(&serving, &registry, |_| Ok(()));
    let before_rejection = generation.capture_configuration();
    let mut observed = serving.clone();
    let text = "invalid-directive \"retained-secret\";";
    let mut hash = Hasher::new();
    hash.update_str(text);
    observed.insert(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(2),
        hash.finish(),
        gatel_kdl::lower(text, "main").unwrap(),
    ));
    assert!(matches!(
        generation.attempt(&observed, &registry, |_| Ok(())),
        Outcome::Rejected(_)
    ));
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal
        .save_checkpoint(&registry, &generation.capture_configuration())
        .unwrap();
    drop(journal);
    let bytes = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    let (mut writer, snapshots) = CheckpointWriter::open(&directory.0, INSTANCE, &registry).unwrap();
    assert!(writer.bind(generation.status().version().boot_id).is_err());
    assert!(
        writer
            .save(&registry, &generation.capture_configuration())
            .is_err()
    );
    assert!(writer.confirm_recovery(&before_rejection).is_err());
    assert!(
        writer
            .confirm_recovery(&committed("different", &registry))
            .is_err()
    );
    assert!(writer.ensure_ready().is_err());
    assert_eq!(std::fs::read(directory.0.join("checkpoint.bin")).unwrap(), bytes);
    let recovered = GenerationStore::<()>::new();
    let (serving, observed) = snapshots.unwrap();
    let mut altered = observed.clone();
    let mut hash = Hasher::new();
    hash.update_str("different rejected content");
    altered.insert(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(2),
        hash.finish(),
        gatel_kdl::lower("invalid-directive \"different\";", "main").unwrap(),
    ));
    let wrong = GenerationStore::<()>::new();
    wrong
        .recover_if(&wrong.status().version(), &serving, &altered, &registry, |_| {
            Ok(())
        })
        .unwrap();
    assert_eq!(altered.revs(), observed.revs());
    assert!(writer.confirm_recovery(&wrong.capture_configuration()).is_err());
    assert!(matches!(
        recovered
            .recover_if(
                &recovered.status().version(),
                &serving,
                &observed,
                &registry,
                |_| Ok(())
            )
            .unwrap(),
        Outcome::Started { .. }
    ));
    writer
        .reserve_recovery(recovered.status().version().boot_id, &serving, &observed)
        .unwrap();
    let competing = GenerationStore::<()>::new();
    competing
        .recover_if(
            &competing.status().version(),
            &serving,
            &observed,
            &registry,
            |_| Ok(()),
        )
        .unwrap();
    assert!(
        writer
            .reserve_recovery(competing.status().version().boot_id, &serving, &observed)
            .is_err()
    );
    assert!(
        writer
            .confirm_recovery(&competing.capture_configuration())
            .is_err()
    );
    writer
        .confirm_recovery(&recovered.capture_configuration())
        .unwrap();
    writer.ensure_ready().unwrap();
    assert!(writer.bind(generation.status().version().boot_id).is_err());
    writer
        .save(&registry, &recovered.capture_configuration())
        .unwrap();
    assert!(
        writer
            .confirm_recovery(&recovered.capture_configuration())
            .is_err(),
        "confirmation is one-shot"
    );
}

#[test]
fn disk_checkpoint_roundtrip_keeps_rejected_full_intent() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("serving-secret", &registry);
    let generation = GenerationStore::<()>::new();
    let mut observed = first.observed_snapshot().clone();
    generation.attempt(&observed, &registry, |_| Ok(()));
    observed.insert(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        Hasher::new().finish(),
        gatel_kdl::lower("invalid-directive \"rejected-secret\";", "admin").unwrap(),
    ));
    assert!(matches!(
        generation.attempt(&observed, &registry, |_| Ok(())),
        Outcome::Rejected(_)
    ));
    let capture = generation.capture_configuration();
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal.save_checkpoint(&registry, &capture).unwrap();
    drop(journal);
    let mut journal = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    let checkpoint = journal.load_checkpoint(&registry).unwrap().unwrap();
    assert_eq!(checkpoint.sources.revs(), capture.committed().unwrap().revs);
    assert_eq!(checkpoint.observed.revs(), capture.version().revs);
    assert_eq!(
        gatel_model::snapshot_codec::encode(&checkpoint.observed).unwrap(),
        gatel_model::snapshot_codec::encode(&observed).unwrap()
    );
    assert!(gatel_plan::normalize(&checkpoint.sources, &registry).is_ok());
    assert!(gatel_plan::normalize(&checkpoint.observed, &registry).is_err());
    assert!(!format!("{checkpoint:?}").contains("secret"));
    let (report, code) = crate::admin::journal::cli::report(&journal.inspect_recovery(&registry).unwrap());
    assert_eq!(code, 0); // Current log coverage, not validation of latest intent.
    assert!(report.contains("checkpoint_serving_sources=1\n"));
    assert!(report.contains("checkpoint_observed_sources=2\n"));
    assert!(report.contains("checkpoint_observed_differs=true\n"));
    assert!(!report.contains("secret"));
    let restored = GenerationStore::<()>::new();
    assert!(matches!(
        restored.attempt(&checkpoint.sources, &registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    let serving = restored.current().unwrap();
    let version = restored
        .restore_intent_if(&restored.status().version(), &checkpoint.observed)
        .unwrap();
    assert_ne!(version.boot_id, capture.version().boot_id);
    assert_eq!(version.revs, capture.version().revs);
    assert!(std::sync::Arc::ptr_eq(&serving, &restored.current().unwrap()));
    assert_eq!(
        gatel_model::snapshot_codec::encode(&restored.observed_snapshot().1).unwrap(),
        gatel_model::snapshot_codec::encode(&observed).unwrap()
    );
    let bootstrap = GenerationStore::<()>::new();
    assert!(matches!(
        bootstrap.recover_if(
            &bootstrap.status().version(),
            &checkpoint.sources,
            &checkpoint.observed,
            &registry,
            |_| {
                assert!(bootstrap.current().is_none());
                assert_eq!(bootstrap.observed_snapshot().1.revs(), observed.revs());
                Ok(())
            },
        ),
        Ok(Outcome::Started { .. })
    ));
    assert_eq!(bootstrap.current().unwrap().plan_hash(), checkpoint.metadata.plan);
    assert_eq!(bootstrap.observed_snapshot().1.revs(), observed.revs());
}

#[test]
fn journal_checkpoint_roundtrip_replaces_without_rewriting_log() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let next = committed("next", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let count = std::fs::read_dir(&directory.0).unwrap().count();
    assert!(journal.load_checkpoint(&registry).unwrap().is_none());
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), count);
    let intent = journal.begin("").unwrap();
    journal.finish(intent, Decision::Applied).unwrap();
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
    assert_eq!(
        journal.load_checkpoint(&registry).unwrap().unwrap().metadata.plan,
        first.committed().unwrap().plan_hash()
    );
    journal.save_checkpoint(&registry, &next).unwrap();
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    drop(journal);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let restored = journal.load_checkpoint(&registry).unwrap().unwrap();
    assert_eq!(restored.metadata.plan, next.committed().unwrap().plan_hash());
    assert_eq!(restored.sources.revs(), next.committed().unwrap().revs);
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 3);
}

#[test]
fn failures_before_and_after_replace_preserve_atomic_visibility_and_poison_writer() {
    let registry = registry();
    let first = committed("first", &registry);
    let next = committed("next", &registry);
    for failure in [
        Phase::Written,
        Phase::Synced,
        Phase::BeforeRename,
        Phase::Renamed,
        Phase::DirectorySynced,
    ] {
        let directory = Directory::new();
        let mut store = open(&directory);
        assert!(store.directory.is_absolute());
        store.save_checkpoint(INSTANCE, &registry, &first).unwrap();
        let path = directory.0.join("checkpoint.bin");
        let before = std::fs::read(&path).unwrap();
        assert!(
            store
                .save_checkpoint_with(INSTANCE, &registry, &next, |phase| {
                    if phase == failure {
                        Err(io::Error::other("injected checkpoint failure"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        );
        assert!(store.poisoned);
        assert!(store.append("next;").is_err());
        assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
        let replaced = matches!(failure, Phase::Renamed | Phase::DirectorySynced);
        assert_eq!(std::fs::read(&path).unwrap() == before, !replaced);
        drop(store);
        let mut store = open(&directory);
        let checkpoint = store.load_checkpoint(INSTANCE, &registry).unwrap().unwrap();
        assert_eq!(
            checkpoint.metadata.plan,
            if replaced {
                next.committed().unwrap().plan_hash()
            } else {
                first.committed().unwrap().plan_hash()
            }
        );
        assert_eq!(
            std::fs::read_dir(&directory.0).unwrap().count(),
            3,
            "temporary file leaked after {failure:?}"
        );
    }
}

#[test]
fn corrupt_oversized_or_nonregular_checkpoint_is_not_silently_replaced() {
    let registry = registry();
    let first = committed("first", &registry);
    let directory = Directory::new();
    let mut store = open(&directory);
    store.save_checkpoint(INSTANCE, &registry, &first).unwrap();
    let path = directory.0.join("checkpoint.bin");
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
    assert!(store.save_checkpoint(INSTANCE, &registry, &first).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(checkpoint::MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
    assert!(store.save_checkpoint(INSTANCE, &registry, &first).is_err());
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        checkpoint::MAX_BYTES as u64 + 1
    );
    assert!(!store.poisoned, "input validation must not poison writer");
    let other = Directory::new();
    let mut store = open(&other);
    std::fs::create_dir(other.0.join("checkpoint.bin")).unwrap();
    assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
    assert!(store.save_checkpoint(INSTANCE, &registry, &first).is_err());
    assert!(other.0.join("checkpoint.bin").is_dir());
}

#[test]
fn unresolved_intent_prevents_checkpoint_write_without_poisoning() {
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let intent = journal.begin("").unwrap();
    assert!(journal.save_checkpoint(&registry, &first).is_err());
    assert!(!directory.0.join("checkpoint.bin").exists());
    journal.finish(intent, Decision::Rejected).unwrap();
    journal.save_checkpoint(&registry, &first).unwrap();
}

#[cfg(unix)]
#[test]
fn unix_checkpoint_permissions_and_symlink_are_enforced() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = Directory::new();
    let registry = registry();
    let first = committed("first", &registry);
    let mut store = open(&directory);
    store.save_checkpoint(INSTANCE, &registry, &first).unwrap();
    let path = directory.0.join("checkpoint.bin");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
    assert!(store.save_checkpoint(INSTANCE, &registry, &first).is_err());
    let other = Directory::new();
    let mut store = open(&other);
    symlink(&path, other.0.join("checkpoint.bin")).unwrap();
    assert!(store.load_checkpoint(INSTANCE, &registry).is_err());
    assert!(store.save_checkpoint(INSTANCE, &registry, &first).is_err());
    assert!(other.0.join("checkpoint.bin").is_symlink());
}
