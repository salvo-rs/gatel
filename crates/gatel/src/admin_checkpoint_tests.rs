use gatel_model::{SourceId, SourceRev, SourceSnapshot};
use gatel_plan::{GenerationStore, Outcome};

use super::*;

#[path = "admin_checkpoint_intent_tests.rs"]
mod intent;

const INSTANCE: &str = "checkpoint-fixture";
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
    let sources = Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        checksum("fixture", text.as_bytes()),
        gatel_kdl::lower(&text, "fixture.kdl").unwrap(),
    ));
    let store = GenerationStore::<()>::new();
    assert!(matches!(
        store.attempt(&sources, registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    store.capture_configuration()
}
fn log() -> Vec<u8> {
    let mut log = super::super::encode(1, &binding::record(INSTANCE).unwrap()).unwrap();
    log.extend(super::super::encode(2, "GATEL-TX1 I\n").unwrap());
    log.extend(super::super::encode(3, "GATEL-TX1 A\n2").unwrap());
    log
}
fn seal(bytes: &mut [u8]) {
    let sum = checksum("gatel.checkpoint.envelope.v2", &bytes[HEADER..]);
    bytes[8..HEADER].copy_from_slice(sum.as_bytes());
}

#[test]
fn envelope_roundtrip_rebuilds_pinned_generation_with_a_new_boot() {
    let registry = registry();
    let original = committed("secret-first", &registry);
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &original).unwrap();
    let checkpoint = decode(&bytes, INSTANCE, &registry, &log).unwrap();
    assert_eq!(checkpoint.metadata.journal_sequence, 3);
    assert_eq!(checkpoint.metadata.journal_bytes, log.len() as u64);
    assert_eq!(checkpoint.sources.revs(), original.committed().unwrap().revs);
    assert!(!format!("{checkpoint:?}").contains("secret-first"));
    let restored = GenerationStore::<()>::new();
    assert!(matches!(
        restored.attempt(&checkpoint.sources, &registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    let restored = restored.capture_configuration();
    assert_eq!(
        restored.committed().unwrap().plan_hash(),
        original.committed().unwrap().plan_hash()
    );
    assert_ne!(restored.version().boot_id, original.version().boot_id);
    let later = decode(
        &encode(INSTANCE, &registry, &log, &restored).unwrap(),
        INSTANCE,
        &registry,
        &log,
    )
    .unwrap();
    assert_ne!(later.metadata.publication, checkpoint.metadata.publication);
}

#[test]
fn instance_registry_and_same_sequence_different_history_are_refused() {
    let registry = registry();
    let original = committed("original", &registry);
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &original).unwrap();
    assert!(decode(&bytes, "other", &registry, &log).is_err());
    let different = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_plan::Bundle::new("other", "1"))
        .build()
        .unwrap();
    assert!(decode(&bytes, INSTANCE, &different, &log).is_err());
    assert!(encode(INSTANCE, &different, &log, &original).is_err());
    let mut changed = super::super::encode(1, &binding::record(INSTANCE).unwrap()).unwrap();
    changed.extend(super::super::encode(2, "GATEL-TX1 I\nother;").unwrap());
    changed.extend(super::super::encode(3, "GATEL-TX1 A\n2").unwrap());
    assert_eq!(log_state(&changed, INSTANCE).unwrap().0.sequence, 3);
    assert!(decode(&bytes, INSTANCE, &registry, &changed).is_err());
}

#[test]
fn later_pending_and_partial_records_do_not_hide_corruption_or_authorize_capture() {
    let registry = registry();
    let original = committed("original", &registry);
    let prefix = log();
    let bytes = encode(INSTANCE, &registry, &prefix, &original).unwrap();
    let mut later = prefix.clone();
    later.extend(super::super::encode(4, "GATEL-TX1 I\nnext;").unwrap());
    assert!(decode(&bytes, INSTANCE, &registry, &later).is_ok());
    assert!(encode(INSTANCE, &registry, &later, &original).is_err());
    later.extend_from_slice(b"GAT");
    assert!(decode(&bytes, INSTANCE, &registry, &later).is_ok());
    assert!(encode(INSTANCE, &registry, &later, &original).is_err());
    *later.last_mut().unwrap() = b'X';
    assert!(decode(&bytes, INSTANCE, &registry, &later).is_err());
    let mut corrupt = prefix.clone();
    corrupt.extend(super::super::encode(4, "GATEL-TX1 A\n2").unwrap());
    assert!(decode(&bytes, INSTANCE, &registry, &corrupt).is_err());
    assert!(decode(&bytes, INSTANCE, &registry, &prefix[..prefix.len() - 1]).is_err());
}

#[test]
fn all_truncations_bit_flips_and_oversize_fail_without_partial_checkpoint() {
    let registry = registry();
    let original = committed("secret-content", &registry);
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &original).unwrap();
    for cut in 0..bytes.len() {
        assert!(
            decode(&bytes[..cut], INSTANCE, &registry, &log).is_err(),
            "cut={cut}"
        );
    }
    for index in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[index] ^= 1;
        assert!(
            decode(&changed, INSTANCE, &registry, &log).is_err(),
            "offset={index}"
        );
    }
    assert!(decode(&vec![0; MAX_BYTES + 1], INSTANCE, &registry, &log).is_err());
    assert!(!invalid().to_string().contains("secret-content"));
}

#[test]
fn correct_checksums_do_not_bypass_prefix_or_recompiled_plan_validation() {
    let registry = registry();
    let original = committed("original", &registry);
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &original).unwrap();
    let mut wrong_sequence = bytes.clone();
    wrong_sequence[HEADER + 128..HEADER + 136].copy_from_slice(&2u64.to_le_bytes());
    seal(&mut wrong_sequence);
    assert!(decode(&wrong_sequence, INSTANCE, &registry, &log).is_err());
    let mut short_prefix = bytes.clone();
    let length = log.len() - 1;
    short_prefix[HEADER + 136..HEADER + 144].copy_from_slice(&(length as u64).to_le_bytes());
    short_prefix[HEADER + 144..HEADER + META]
        .copy_from_slice(checksum("gatel.checkpoint.journal-prefix.v1", &log[..length]).as_bytes());
    seal(&mut short_prefix);
    assert!(decode(&short_prefix, INSTANCE, &registry, &log).is_err());
    let different = committed("different", &registry);
    let mut wrong_sources = bytes[..HEADER + META].to_vec();
    let sources = snapshot_codec::encode(different.committed().unwrap().source_snapshot()).unwrap();
    wrong_sources.extend((sources.len() as u64).to_le_bytes());
    wrong_sources.extend(&sources);
    wrong_sources.extend(&sources);
    seal(&mut wrong_sources);
    assert!(decode(&wrong_sources, INSTANCE, &registry, &log).is_err());
}
