use gatel_model::{Generation, Node, Origin, SourceRev, SourceSnapshot};

use super::*;

fn snapshot(rev: u64, name: &str) -> Snapshot {
    Snapshot::new()
        .with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(1),
            Hasher::new().finish(),
            vec![],
        ))
        .with(SourceSnapshot::new(
            SourceId::admin(),
            SourceRev(rev),
            checksum("fixture", name.as_bytes()),
            vec![Node::new(name, Origin::synthetic("private-origin"))],
        ))
}
fn version(base: &Snapshot) -> Version {
    Version {
        boot_id: BootId::generate(),
        generation: Generation::NONE,
        revs: base.revs(),
    }
}
fn build() -> Fingerprint {
    checksum("fixture", b"build")
}
fn seal(bytes: &mut [u8]) {
    let hash = checksum("gatel.wal.intent-envelope.v1", &bytes[HEADER..]);
    bytes[8..HEADER].copy_from_slice(hash.as_bytes());
}

#[test]
fn roundtrip_preserves_full_rejected_intent_and_context_without_exposing_keys() {
    let base = snapshot(1, "base-private");
    let candidate = snapshot(2, "invalid-private");
    let version = version(&base);
    let request = RequestId::client(&SourceId::admin(), "private-client-key").unwrap();
    let bytes = encode("instance", build(), request, &version, &base, &candidate).unwrap();
    assert_eq!(
        bytes,
        encode("instance", build(), request, &version, &base, &candidate).unwrap()
    );
    let decoded = decode(&bytes, "instance", build(), &base).unwrap();
    assert_eq!(decoded.request, request);
    assert_eq!(
        decoded.base_version,
        checksum("gatel.wal.base-version.v1", version.etag().as_bytes())
    );
    assert_eq!(
        source_bytes(&decoded.sources).unwrap(),
        source_bytes(&candidate).unwrap()
    );
    assert!(!format!("{decoded:?} {request:?}").contains("private"));
    assert!(!bytes.windows(18).any(|part| part == b"private-client-key"));
    let changed = encode(
        "instance",
        build(),
        request,
        &version,
        &base,
        &snapshot(2, "different"),
    )
    .unwrap();
    assert_ne!(
        decode(&changed, "instance", build(), &base).unwrap().identity,
        decoded.identity
    );
}

#[test]
fn request_keys_are_scoped_case_sensitive_bounded_and_separate_from_host_ids() {
    let source = SourceId::admin();
    let key = RequestId::client(&source, "key").unwrap();
    assert_ne!(key, RequestId::client(&source, "KEY").unwrap());
    assert_ne!(key, RequestId::client(&SourceId::file("main"), "key").unwrap());
    assert_ne!(RequestId::generate(), RequestId::generate());
    assert_ne!(key, RequestId::generate());
    assert!(RequestId::client(&source, &"x".repeat(128)).is_ok());
    for value in [
        "".to_owned(),
        " ".into(),
        "é".into(),
        "x\n".into(),
        "x".repeat(129),
    ] {
        assert_eq!(RequestId::client(&source, &value), Err(Error::InvalidKey));
    }
}

#[test]
fn resealed_lengths_trailing_bytes_and_oversized_envelopes_are_rejected() {
    let base = snapshot(1, "base");
    let bytes = encode(
        "instance",
        build(),
        RequestId::generate(),
        &version(&base),
        &base,
        &snapshot(2, "next"),
    )
    .unwrap();
    for length in [0, 1, u64::MAX] {
        let mut corrupt = bytes.clone();
        corrupt[HEADER + META..HEADER + META + LENGTH].copy_from_slice(&length.to_le_bytes());
        seal(&mut corrupt);
        assert!(matches!(
            decode(&corrupt, "instance", build(), &base),
            Err(Error::Invalid)
        ));
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    seal(&mut trailing);
    assert!(matches!(
        decode(&trailing, "instance", build(), &base),
        Err(Error::Invalid)
    ));
    let length = (trailing.len() - HEADER - META - LENGTH) as u64;
    trailing[HEADER + META..HEADER + META + LENGTH].copy_from_slice(&length.to_le_bytes());
    seal(&mut trailing);
    assert!(matches!(
        decode(&trailing, "instance", build(), &base),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        decode(&vec![0; MAX_BYTES + 1], "instance", build(), &base),
        Err(Error::Limit)
    ));
}

#[test]
fn resealed_zero_regressed_and_reused_revisions_fail_history_validation() {
    let base = snapshot(2, "base");
    let bytes = encode(
        "instance",
        build(),
        RequestId::generate(),
        &version(&base),
        &base,
        &snapshot(3, "next"),
    )
    .unwrap();
    for candidate in [
        snapshot(0, "zero"),
        snapshot(1, "regressed"),
        snapshot(2, "reused"),
    ] {
        assert_eq!(
            encode(
                "instance",
                build(),
                RequestId::generate(),
                &version(&base),
                &base,
                &candidate
            ),
            Err(Error::History)
        );
        let payload = source_bytes(&candidate).unwrap();
        let mut forged = bytes[..HEADER + META].to_vec();
        forged.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        forged.extend_from_slice(&payload);
        seal(&mut forged);
        assert!(matches!(
            decode(&forged, "instance", build(), &base),
            Err(Error::History)
        ));
    }
}

#[test]
fn wrong_context_damaged_length_truncation_and_resealed_bad_history_are_rejected() {
    let base = snapshot(1, "base");
    let candidate = snapshot(2, "candidate");
    let bytes = encode(
        "instance",
        build(),
        RequestId::generate(),
        &version(&base),
        &base,
        &candidate,
    )
    .unwrap();
    assert!(matches!(
        decode(&bytes, "other", build(), &base),
        Err(Error::Context)
    ));
    assert!(matches!(
        decode(&bytes, "instance", checksum("other", b"build"), &base),
        Err(Error::Context)
    ));
    assert!(matches!(
        decode(
            &bytes,
            "instance",
            build(),
            &snapshot(1, "same-rev-different-content")
        ),
        Err(Error::Context)
    ));
    for cut in 0..bytes.len() {
        assert!(decode(&bytes[..cut], "instance", build(), &base).is_err());
    }
    for offset in [0, 8, HEADER, HEADER + META, bytes.len() - 1] {
        let mut corrupt = bytes.clone();
        corrupt[offset] ^= 1;
        assert!(decode(&corrupt, "instance", build(), &base).is_err());
    }
    let mut forged = bytes[..HEADER + META].to_vec();
    let payload = source_bytes(&Snapshot::new()).unwrap();
    forged.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    forged.extend_from_slice(&payload);
    seal(&mut forged);
    assert!(matches!(
        decode(&forged, "instance", build(), &base),
        Err(Error::History)
    ));
    let mut mismatched = version(&base);
    mismatched.revs = Snapshot::new().revs();
    assert_eq!(
        encode(
            "instance",
            build(),
            RequestId::generate(),
            &mismatched,
            &base,
            &candidate
        ),
        Err(Error::Context)
    );
}
