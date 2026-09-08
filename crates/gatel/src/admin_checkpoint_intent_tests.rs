use super::*;

#[test]
fn rejected_intent_and_later_noop_tombstone_survive_independently_of_serving_sources() {
    let registry = registry();
    let first = committed("serving-secret", &registry);
    let store = GenerationStore::<()>::new();
    let mut input = first.committed().unwrap().source_snapshot().clone();
    store.attempt(&input, &registry, |_| Ok(()));
    let admin = SourceId::admin();
    input.insert(SourceSnapshot::new(
        admin.clone(),
        SourceRev(1),
        checksum("test", b"rejected-secret"),
        gatel_kdl::lower("unknown-directive \"rejected-secret\";", "admin").unwrap(),
    ));
    assert!(matches!(
        store.attempt(&input, &registry, |_| Ok(())),
        Outcome::Rejected(_)
    ));
    let capture = store.capture_configuration();
    let log = log();
    let rejected = decode(
        &encode(INSTANCE, &registry, &log, &capture).unwrap(),
        INSTANCE,
        &registry,
        &log,
    )
    .unwrap();
    assert!(normalize(&rejected.sources, &registry).is_ok());
    assert!(normalize(&rejected.observed, &registry).is_err());
    assert!(rejected.sources.get(&admin).is_none());
    assert_eq!(rejected.observed.get(&admin).unwrap().rev, SourceRev(1));
    assert_eq!(
        snapshot_codec::encode(&rejected.observed).unwrap(),
        snapshot_codec::encode(&input).unwrap()
    );
    assert!(!format!("{rejected:?}").contains("secret"));
    input.insert(SourceSnapshot::new(
        admin.clone(),
        SourceRev(2),
        checksum("test", b"empty"),
        vec![],
    ));
    assert!(matches!(
        store.attempt(&input, &registry, |_| panic!("NoOp must not prepare")),
        Outcome::NoOp { .. }
    ));
    let capture = store.capture_configuration();
    let noop = decode(
        &encode(INSTANCE, &registry, &log, &capture).unwrap(),
        INSTANCE,
        &registry,
        &log,
    )
    .unwrap();
    assert!(noop.sources.get(&admin).is_none());
    assert!(noop.observed.get(&admin).unwrap().nodes.is_empty());
    assert_eq!(noop.observed.get(&admin).unwrap().rev, SourceRev(2));
    assert_eq!(noop.metadata.publication, rejected.metadata.publication);
    assert_eq!(noop.metadata.plan, rejected.metadata.plan);
}

fn replace_observed(bytes: &[u8], observed: &Snapshot) -> Vec<u8> {
    let length =
        u64::from_le_bytes(bytes[HEADER + META..HEADER + META + LENGTH].try_into().unwrap()) as usize;
    let mut changed = bytes[..HEADER + META + LENGTH + length].to_vec();
    changed.extend(snapshot_codec::encode(observed).unwrap());
    seal(&mut changed);
    changed
}

#[test]
fn valid_checksums_do_not_authorize_inconsistent_source_history_or_length() {
    let registry = registry();
    let first = committed("secret-body", &registry);
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &first).unwrap();
    let source = first.observed_snapshot().sources().next().unwrap().clone();
    let mut invalid = vec![Snapshot::new()];
    let mut changed = source.clone();
    changed.rev = SourceRev(0);
    invalid.push(Snapshot::new().with(changed));
    let mut changed = source.clone();
    changed.content = checksum("test", b"different-content");
    invalid.push(Snapshot::new().with(changed));
    let mut changed = source;
    changed.nodes.clear();
    invalid.push(Snapshot::new().with(changed));
    invalid.push(first.observed_snapshot().clone().with(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(0),
        checksum("test", b"zero"),
        vec![],
    )));
    for observed in invalid {
        assert!(decode(&replace_observed(&bytes, &observed), INSTANCE, &registry, &log).is_err());
    }
    for length in [0, 1, u64::MAX] {
        let mut changed = bytes.clone();
        changed[HEADER + META..HEADER + META + LENGTH].copy_from_slice(&length.to_le_bytes());
        seal(&mut changed);
        assert!(decode(&changed, INSTANCE, &registry, &log).is_err());
    }
    let mut legacy = bytes.clone();
    legacy[..8].copy_from_slice(b"GATELC01");
    assert!(decode(&legacy, INSTANCE, &registry, &log).is_err());
    let empty = GenerationStore::<()>::new().capture_configuration();
    assert!(encode(INSTANCE, &registry, &log, &empty).is_err());
}

#[test]
fn higher_published_revision_cannot_be_replaced_by_older_observed_revision() {
    let registry = registry();
    let first = committed("body", &registry);
    let store = GenerationStore::<()>::new();
    let mut source = first.observed_snapshot().sources().next().unwrap().clone();
    source.rev = SourceRev(4);
    store.attempt(&Snapshot::new().with(source.clone()), &registry, |_| Ok(()));
    let log = log();
    let bytes = encode(INSTANCE, &registry, &log, &store.capture_configuration()).unwrap();
    source.rev = SourceRev(3);
    let changed = replace_observed(&bytes, &Snapshot::new().with(source));
    assert!(decode(&changed, INSTANCE, &registry, &log).is_err());
}
