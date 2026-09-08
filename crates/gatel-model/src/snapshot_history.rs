//! Shared full-source history validation for recovery and persistence.
use crate::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotHistoryError {
    ZeroRevision,
    MissingSource,
    RevisionRegression,
    RevisionReused,
}

impl std::fmt::Display for SnapshotHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ZeroRevision => "source history contains a zero revision",
            Self::MissingSource => "source history omits a previously admitted source",
            Self::RevisionRegression => "source history moves a revision backwards",
            Self::RevisionReused => "source history reuses a revision with different content",
        })
    }
}
impl std::error::Error for SnapshotHistoryError {}

impl Snapshot {
    /// Check that `next` retains every source and moves revisions only forward.
    /// Equal revisions require exact content identity and node trees, including
    /// origins, order and float bits. Newer empty contributions are tombstones.
    ///
    /// No normalization, source authorization, source count/codec budget, CAS,
    /// publication or I/O is performed. Invalid semantic intent is valid history.
    /// Gaps in revisions are allowed; this is not proof of intervening writes.
    /// Errors intentionally omit source identities and configuration content.
    pub fn validate_successor(&self, next: &Self) -> Result<(), SnapshotHistoryError> {
        if self
            .sources()
            .chain(next.sources())
            .any(|source| source.rev.0 == 0)
        {
            return Err(SnapshotHistoryError::ZeroRevision);
        }
        for old in self.sources() {
            let new = next.get(&old.source).ok_or(SnapshotHistoryError::MissingSource)?;
            if new.rev < old.rev {
                return Err(SnapshotHistoryError::RevisionRegression);
            }
            if new.rev == old.rev
                && (new.content != old.content || !crate::value_identity::same_nodes(&old.nodes, &new.nodes))
            {
                return Err(SnapshotHistoryError::RevisionReused);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hasher, Node, Origin, SourceId, SourceRev, SourceSnapshot, Value};

    fn snapshot(revision: u64) -> Snapshot {
        Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("private-source"),
            SourceRev(revision),
            Hasher::new().finish(),
            vec![Node::new(
                "unknown-directive",
                Origin::synthetic("private-origin"),
            )],
        ))
    }

    #[test]
    fn history_preserves_sources_and_allows_newer_tombstones_without_compiling() {
        let first = snapshot(1);
        assert!(Snapshot::new().validate_successor(&first).is_ok());
        assert!(first.validate_successor(&first).is_ok());
        assert_eq!(
            first.validate_successor(&Snapshot::new()),
            Err(SnapshotHistoryError::MissingSource)
        );
        assert_eq!(
            snapshot(3).validate_successor(&first),
            Err(SnapshotHistoryError::RevisionRegression)
        );
        assert_eq!(
            snapshot(0).validate_successor(&first),
            Err(SnapshotHistoryError::ZeroRevision)
        );
        assert_eq!(
            first.validate_successor(&snapshot(0)),
            Err(SnapshotHistoryError::ZeroRevision)
        );
        let mut source = first.sources().next().unwrap().clone();
        source.rev = SourceRev(9);
        source.nodes.clear();
        let next = Snapshot::new().with(source).with(SourceSnapshot::new(
            SourceId::admin(),
            SourceRev(1),
            Hasher::new().finish(),
            vec![],
        ));
        assert!(first.validate_successor(&next).is_ok());
    }

    #[test]
    fn reused_revision_checks_identity_origin_and_float_bits_without_serialization() {
        let first = snapshot(1);
        for edit in 0..3 {
            let mut source = first.sources().next().unwrap().clone();
            match edit {
                0 => {
                    let mut hash = Hasher::new();
                    hash.update_str("different");
                    source.content = hash.finish();
                }
                1 => source.nodes[0].origin = Origin::synthetic("different-private-origin"),
                _ => source.nodes[0].name.push('x'),
            }
            let error = first
                .validate_successor(&Snapshot::new().with(source))
                .unwrap_err();
            assert_eq!(error, SnapshotHistoryError::RevisionReused);
            assert!(!format!("{error:?} {error}").contains("private"));
        }
        let mut source = first.sources().next().unwrap().clone();
        source.nodes[0]
            .args
            .push(Value::new(crate::Scalar::Float(-0.0), Origin::synthetic("float")));
        let negative = Snapshot::new().with(source.clone());
        assert!(negative.validate_successor(&negative).is_ok());
        source.nodes[0].args[0].scalar = crate::Scalar::Float(0.0);
        assert_eq!(
            negative.validate_successor(&Snapshot::new().with(source)),
            Err(SnapshotHistoryError::RevisionReused)
        );
    }
}
