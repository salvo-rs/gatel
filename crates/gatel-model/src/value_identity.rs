//! Exact frontend observation identity, not numerical or semantic equivalence.
use crate::{Node, Scalar, Value};

fn value(left: &Value, right: &Value) -> bool {
    left.origin == right.origin
        && match (&left.scalar, &right.scalar) {
            (Scalar::Float(left), Scalar::Float(right)) => left.to_bits() == right.to_bits(),
            (left, right) => left == right,
        }
}

pub(crate) fn same_nodes(left: &[Node], right: &[Node]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut pending: Vec<_> = left.iter().zip(right).collect();
    while let Some((left, right)) = pending.pop() {
        if left.name != right.name
            || left.origin != right.origin
            || left.args.len() != right.args.len()
            || left.props.len() != right.props.len()
            || left.children.len() != right.children.len()
            || !left
                .args
                .iter()
                .zip(&right.args)
                .all(|(left, right)| value(left, right))
            || !left.props.iter().zip(&right.props).all(|(left, right)| {
                left.name == right.name && left.origin == right.origin && value(&left.value, &right.value)
            })
        {
            return false;
        }
        pending.extend(left.children.iter().zip(&right.children));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hasher, ObservationError, Origin, Snapshot, SourceId, SourceRev};

    #[test]
    fn exact_identity_includes_all_node_fields_origins_and_order() {
        let origin = Origin::synthetic("one");
        let node = Node::new("node", origin.clone())
            .with_arg(Value::int(1, origin.clone()))
            .with_prop("key", Value::bool(true, origin.clone()))
            .with_child(Node::new("child", origin.clone()));
        assert!(same_nodes(
            std::slice::from_ref(&node),
            std::slice::from_ref(&node)
        ));
        for edit in 0..10 {
            let mut changed = node.clone();
            match edit {
                0 => changed.name.push('x'),
                1 => changed.origin = Origin::synthetic("other"),
                2 => changed.args[0].scalar = Scalar::Int(2),
                3 => changed.args[0].origin = Origin::synthetic("other"),
                4 => changed.props[0].name.push('x'),
                5 => changed.props[0].origin = Origin::synthetic("other"),
                6 => changed.props[0].value.origin = Origin::synthetic("other"),
                7 => changed.props[0].value.scalar = Scalar::Bool(false),
                8 => changed.children[0].name.push('x'),
                _ => changed.children.push(Node::new("extra", origin.clone())),
            }
            assert!(
                !same_nodes(std::slice::from_ref(&node), &[changed]),
                "edit {edit}"
            );
        }
        let other = Node::new("other", origin);
        assert!(!same_nodes(&[node.clone(), other.clone()], &[other, node]));
    }

    #[test]
    fn repeated_nan_bits_are_identical_but_signed_zero_and_nan_payloads_are_not() {
        let node = |bits| {
            Node::new("n", Origin::synthetic("float")).with_arg(Value::new(
                Scalar::Float(f64::from_bits(bits)),
                Origin::synthetic("float"),
            ))
        };
        let nan = 0x7ff8_0000_0000_0001;
        assert!(same_nodes(&[node(nan)], &[node(nan)]));
        assert!(!same_nodes(&[node(nan)], &[node(nan + 1)]));
        assert!(!same_nodes(&[node(0)], &[node(1 << 63)]));
    }

    #[test]
    fn mismatched_repeated_identity_leaves_composed_snapshot_untouched() {
        let mut snapshot = Snapshot::new();
        let source = SourceId::admin();
        let hash = Hasher::new().finish();
        let original = vec![Node::new("original", Origin::synthetic("snapshot"))];
        snapshot
            .observe_if(source.clone(), None, hash, original.clone())
            .unwrap();
        let outcome = snapshot.observe_if(
            source.clone(),
            Some(SourceRev(1)),
            hash,
            vec![Node::new("changed", Origin::synthetic("snapshot"))],
        );
        assert_eq!(
            outcome,
            Err(crate::SourceUpdateError::Observation(
                ObservationError::ContentMismatch
            ))
        );
        assert_eq!(snapshot.get(&source).unwrap().rev, SourceRev(1));
        assert_eq!(snapshot.get(&source).unwrap().nodes, original);
    }
}
