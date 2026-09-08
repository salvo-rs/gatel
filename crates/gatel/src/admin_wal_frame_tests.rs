use super::*;

fn scan_only(bytes: &[u8], base: u64) -> Result<Scan, Error> {
    scan(bytes, base, |_| Ok(()))
}
fn seal(bytes: &mut [u8]) {
    let hash = checksum("gatel.wal.frame-header.v1", &bytes[..64]);
    bytes[64..HEADER].copy_from_slice(hash.as_bytes());
}

#[test]
fn binary_frames_roundtrip_with_explicit_base_and_redacted_debug() {
    let payload = b"private\0\xff\xfe";
    let first = encode(11, Kind::Intent, payload).unwrap();
    let second = encode(12, Kind::Decision, &[0; DECISION_BYTES]).unwrap();
    let bytes = [first, second].concat();
    let mut seen = Vec::new();
    let result = scan(&bytes, 10, |frame| {
        assert!(!format!("{frame:?}").contains("private"));
        seen.push((frame.sequence, frame.kind, frame.payload));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![
            (11, Kind::Intent, payload.as_slice()),
            (12, Kind::Decision, [0; DECISION_BYTES].as_slice())
        ]
    );
    assert_eq!(
        result,
        Scan {
            sequence: 12,
            valid_bytes: bytes.len(),
            incomplete_tail: false
        }
    );
    assert_eq!(scan_only(&bytes, 0), Err(Error::Sequence));
}

#[test]
fn every_truncated_frame_preserves_only_complete_prefix() {
    let first = encode(1, Kind::Intent, b"candidate").unwrap();
    let second = encode(2, Kind::Decision, &[0; DECISION_BYTES]).unwrap();
    for cut in 1..second.len() {
        let bytes = [first.as_slice(), &second[..cut]].concat();
        assert_eq!(
            scan_only(&bytes, 0).unwrap(),
            Scan {
                sequence: 1,
                valid_bytes: first.len(),
                incomplete_tail: true
            }
        );
    }
    assert!(!scan_only(&first, 0).unwrap().incomplete_tail);
    assert_eq!(scan_only(&[], u64::MAX).unwrap().sequence, u64::MAX);
}

#[test]
fn corruption_never_masquerades_as_a_short_payload() {
    let frame = encode(1, Kind::Intent, b"candidate").unwrap();
    for offset in 0..frame.len() {
        let mut bad = frame.clone();
        bad[offset] ^= 1;
        assert!(scan_only(&bad, 0).is_err(), "offset {offset}");
    }
    for offset in [16, 17] {
        let mut bad = frame.clone();
        bad[offset] = 99;
        seal(&mut bad);
        assert_eq!(scan_only(&bad, 0), Err(Error::Header));
    }
    let mut bad = frame.clone();
    bad[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    seal(&mut bad);
    assert_eq!(scan_only(&bad, 0), Err(Error::Length));
    assert_eq!(scan_only(&frame, u64::MAX), Err(Error::Sequence));
    assert_eq!(
        scan_only(&[frame.clone(), frame].concat(), 0),
        Err(Error::Sequence)
    );
}

#[test]
fn capacity_reserves_decision_and_sequence_without_overflow() {
    let payload = wal_intent::MAX_BYTES;
    let used = MAX_LOG - HEADER - payload - DECISION_RESERVE;
    assert_eq!(reserve_intent(used, u64::MAX - 2, payload), Ok(()));
    assert_eq!(reserve_intent(used + 1, 0, payload), Err(Error::Limit));
    assert_eq!(reserve_intent(usize::MAX, 0, payload), Err(Error::Limit));
    assert_eq!(reserve_intent(0, u64::MAX - 1, 1), Err(Error::Sequence));
    assert_eq!(reserve_intent(0, 0, 0), Err(Error::Length));
    assert_eq!(reserve_intent(0, 0, payload + 1), Err(Error::Length));
    assert_eq!(encode(0, Kind::Intent, b"x"), Err(Error::Sequence));
    assert_eq!(
        encode(1, Kind::Decision, &[0; DECISION_BYTES - 1]),
        Err(Error::Length)
    );
    assert_eq!(scan_only(&vec![0; MAX_LOG + 1], 0), Err(Error::Limit));
}

#[test]
fn visitor_errors_abort_scan_without_implying_commit() {
    let bytes = encode(1, Kind::Intent, b"x").unwrap();
    assert_eq!(scan(&bytes, 0, |_| Err(Error::Payload)), Err(Error::Payload));
}
