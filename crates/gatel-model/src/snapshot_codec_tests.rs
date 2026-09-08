use super::*;

fn source(nodes: Vec<Node>) -> Snapshot {
    Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(7),
        Fingerprint::from_bytes([29; 32]),
        nodes,
    ))
}
fn seal(bytes: &mut [u8]) {
    let sum = checksum(&bytes[HEADER..]);
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..HEADER].copy_from_slice(sum.as_bytes());
}
fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; HEADER];
    bytes.extend_from_slice(body);
    seal(&mut bytes);
    bytes
}

#[test]
fn all_scalar_bits_origins_order_and_empty_sources_roundtrip() {
    let origins = [
        Origin::file("配置.kdl", Span::new(123, 456, 7, 8))
            .field("route")
            .field("proxy"),
        Origin::api("admin", "/routes/0").field("args.1"),
        Origin::synthetic("derived-default").field("timeout"),
    ];
    let scalars = [
        Scalar::Null,
        Scalar::Bool(false),
        Scalar::Bool(true),
        Scalar::Int(i64::MIN),
        Scalar::Int(i64::MAX),
        Scalar::Float(-0.0),
        Scalar::Float(f64::INFINITY),
        Scalar::Float(f64::NEG_INFINITY),
        Scalar::Float(f64::from_bits(0x7ff8000000000123)),
        Scalar::Float(f64::from_bits(1)),
        Scalar::Str("secret\n配置\0".into()),
    ];
    let mut node = Node::new("node", origins[0].clone());
    for (index, scalar) in scalars.into_iter().enumerate() {
        node.args.push(Value::new(scalar, origins[index % 3].clone()));
    }
    for origin in &origins {
        node.props.push(Property {
            name: "duplicate".into(),
            origin: origin.clone(),
            value: Value::int(1, origins[1].clone()),
        });
    }
    node.children.push(Node::new("first", origins[2].clone()));
    node.children.push(Node::new("second", origins[1].clone()));
    let snapshot = source(vec![node])
        .with(SourceSnapshot::new(
            SourceId::admin(),
            SourceRev(0),
            Fingerprint::ZERO,
            vec![],
        ))
        .with(SourceSnapshot::new(
            SourceId::kubernetes("space"),
            SourceRev(u64::MAX),
            Fingerprint::from_bytes([255; 32]),
            vec![],
        ));
    let bytes = encode(&snapshot).unwrap();
    let restored = decode(&bytes).unwrap();
    assert_eq!(restored.revs(), snapshot.revs());
    assert_eq!(restored.sources().count(), 3);
    for old in snapshot.sources() {
        let new = restored.get(&old.source).unwrap();
        assert_eq!(old.content, new.content);
        assert!(crate::value_identity::same_nodes(&old.nodes, &new.nodes));
    }
    assert_eq!(encode(&restored).unwrap(), bytes);
    assert!(decode(&encode(&Snapshot::new()).unwrap()).unwrap().is_empty());
}

#[test]
fn every_truncation_and_single_byte_corruption_fails_without_partial_result() {
    let bytes = encode(&source(vec![Node::new("secret", Origin::synthetic("fixture"))])).unwrap();
    for cut in 0..bytes.len() {
        assert!(decode(&bytes[..cut]).is_err(), "cut={cut}");
    }
    for index in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[index] ^= 1;
        assert!(decode(&changed).is_err(), "offset={index}");
    }
    assert!(!Error::Invalid.to_string().contains("secret"));
}

#[test]
fn checksummed_unknown_tags_invalid_utf8_trailing_and_duplicate_sources_fail() {
    let bytes = encode(&source(vec![])).unwrap();
    let mut unknown = bytes.clone();
    unknown[HEADER + 4] = 255;
    seal(&mut unknown);
    assert!(decode(&unknown).is_err());
    let mut utf8 = bytes.clone();
    utf8[HEADER + 9] = 255;
    seal(&mut utf8);
    assert!(decode(&utf8).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    seal(&mut trailing);
    assert!(decode(&trailing).is_err());
    let mut duplicate = frame(&2u32.to_le_bytes());
    duplicate.extend_from_slice(&bytes[HEADER + 4..]);
    duplicate.extend_from_slice(&bytes[HEADER + 4..]);
    seal(&mut duplicate);
    assert!(decode(&duplicate).is_err());
    // A valid checksum is not authority to invent an admin namespace.
    let mut bad_admin = bytes;
    bad_admin[HEADER + 4] = 1;
    seal(&mut bad_admin);
    assert!(decode(&bad_admin).is_err());
}

#[test]
fn depth_limit_is_symmetric_and_deep_input_is_rejected() {
    let mut node = Node::new("leaf", Origin::synthetic("depth"));
    for _ in 1..MAX_DEPTH {
        node = Node::new("parent", Origin::synthetic("depth")).with_child(node);
    }
    let valid = source(vec![node]);
    let bytes = encode(&valid).unwrap();
    assert!(decode(&bytes).is_ok());
    let mut invalid = valid;
    let mut input = invalid.get(&SourceId::file("main")).unwrap().clone();
    input.nodes = vec![Node::new("too-deep", Origin::synthetic("depth")).with_child(input.nodes.remove(0))];
    invalid.insert(input);
    assert_eq!(encode(&invalid).unwrap_err(), Error::Limit);
    // Build a checksummed 65-level wire tree without the encoder's node guard.
    // Exercise the public decoder, not a manually chosen internal depth value.
    let mut writer = Writer {
        bytes: vec![0; HEADER],
        budget: Budget::default(),
    };
    writer.count(1, MAX_SOURCES).unwrap();
    writer.tag(0).unwrap();
    writer.string("main").unwrap();
    writer.u64(1).unwrap();
    writer.raw(Fingerprint::ZERO.as_bytes()).unwrap();
    writer.count(1, MAX_NODES).unwrap();
    for level in 0..=MAX_DEPTH {
        writer.string("nested").unwrap();
        writer.origin(&Origin::synthetic("depth")).unwrap();
        writer.count(0, MAX_VALUES).unwrap();
        writer.count(0, MAX_VALUES).unwrap();
        writer.count(usize::from(level < MAX_DEPTH), MAX_NODES).unwrap();
    }
    seal(&mut writer.bytes);
    assert_eq!(decode(&writer.bytes).unwrap_err(), Error::Limit);
}

#[test]
fn bytes_strings_counts_and_aggregate_budgets_are_bounded() {
    assert_eq!(decode(&vec![0; MAX_BYTES + 1]).unwrap_err(), Error::Limit);
    assert_eq!(
        decode(&frame(&((MAX_SOURCES + 1) as u32).to_le_bytes())).unwrap_err(),
        Error::Limit
    );
    let large = "x".repeat(MAX_STRING);
    let snapshot = source(vec![Node::new(&large, Origin::synthetic("limit"))]);
    assert!(decode(&encode(&snapshot).unwrap()).is_ok());
    assert_eq!(
        encode(&source(vec![Node::new(large + "x", Origin::synthetic("limit"))])).unwrap_err(),
        Error::Limit
    );
    let mut writer = Writer {
        bytes: vec![0; MAX_BYTES - 1],
        budget: Budget::default(),
    };
    assert_eq!(writer.raw(&[0, 1]), Err(Error::Limit));
    assert_eq!(writer.bytes.len(), MAX_BYTES - 1);
    let mut budget = Budget {
        nodes: MAX_NODES,
        values: MAX_VALUES,
    };
    assert_eq!(budget.node(1), Err(Error::Limit));
    assert_eq!(budget.values(1), Err(Error::Limit));
    let count = ((MAX_VALUES + 1) as u32).to_le_bytes();
    let mut reader = Reader {
        bytes: &count,
        budget: Budget::default(),
    };
    assert_eq!(reader.count(MAX_VALUES), Err(Error::Limit));
}

#[test]
fn source_order_is_canonical_not_silently_normalized_on_decode() {
    let first = encode(&Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("a"),
        SourceRev(1),
        Fingerprint::ZERO,
        vec![],
    )))
    .unwrap();
    let second = encode(&Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("b"),
        SourceRev(1),
        Fingerprint::ZERO,
        vec![],
    )))
    .unwrap();
    let mut unordered = frame(&2u32.to_le_bytes());
    unordered.extend_from_slice(&second[HEADER + 4..]);
    unordered.extend_from_slice(&first[HEADER + 4..]);
    seal(&mut unordered);
    assert!(decode(&unordered).is_err());
}
