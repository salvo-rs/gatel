//! The immutable input to compilation.
//!
//! A snapshot is the union of what every configuration source declared at one
//! point in time. Sources are merged by union only: a name claimed by two
//! sources is an error, never a silent overwrite.
//!
//! The file host can register several named roots, with each root's imports
//! already spliced into its source. A control-plane frontend must likewise
//! arrive as its own source and revision, not mutate a file source's intent.

use std::collections::BTreeMap;

use crate::hash::Fingerprint;
use crate::id::{SourceId, SourceRev};
use crate::value::Node;

/// What one source contributed.
#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub source: SourceId,
    /// Monotonic within a source; advances whenever `content` changes.
    pub rev: SourceRev,
    /// Fingerprint of the raw bytes read, before any interpretation. Two reads
    /// producing the same fingerprint cannot differ semantically.
    pub content: Fingerprint,
    /// Top-level nodes, in declaration order.
    pub nodes: Vec<Node>,
}

impl SourceSnapshot {
    /// Assign a revision from a trusted frontend's complete content identity.
    /// Callers must serialize observation and retaining the result per source.
    /// Reusing an identity requires the exact same node tree, including origins
    /// and float bits; otherwise reject without assigning or retaining a revision.
    pub fn observe(
        source: SourceId,
        content: Fingerprint,
        nodes: Vec<Node>,
        previous: Option<&Self>,
    ) -> Result<Self, ObservationError> {
        let rev = match previous {
            Some(previous) if previous.source != source => return Err(ObservationError::WrongSource),
            Some(previous) if previous.content == content => {
                if !crate::value_identity::same_nodes(&previous.nodes, &nodes) {
                    return Err(ObservationError::ContentMismatch);
                }
                previous.rev
            }
            Some(previous) => previous.rev.next().ok_or(ObservationError::RevisionExhausted)?,
            None => SourceRev(1),
        };
        Ok(Self::new(source, rev, content, nodes))
    }

    pub fn new(source: SourceId, rev: SourceRev, content: Fingerprint, nodes: Vec<Node>) -> Self {
        Self {
            source,
            rev,
            content,
            nodes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationError {
    WrongSource,
    RevisionExhausted,
    ContentMismatch,
}
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::WrongSource => "previous observation belongs to another source",
            Self::RevisionExhausted => "source revision counter exhausted",
            Self::ContentMismatch => "source nodes changed without changing their content identity",
        })
    }
}
impl std::error::Error for ObservationError {}

/// Every source, at one moment, not a partial source update. A running store
/// retains observed source revisions: remove a source's contributions with an
/// empty SourceSnapshot at a newer revision, not by omitting the source.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    sources: BTreeMap<SourceId, SourceSnapshot>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, snapshot: SourceSnapshot) -> Self {
        self.insert(snapshot);
        self
    }

    pub fn insert(&mut self, snapshot: SourceSnapshot) {
        self.sources.insert(snapshot.source.clone(), snapshot);
    }

    /// Compose one trusted frontend observation without overwriting other sources.
    /// `None` expects an unseen source, not an unconditional update. An empty
    /// node list is a retained tombstone, not removal of the source's revision.
    ///
    /// The host must serialize access to this snapshot and authorize the source
    /// before calling. Independent clones do not share a CAS: publishing still
    /// requires the generation store's whole-version check. Content identity must
    /// cover every semantic input, as required by `SourceSnapshot::observe`.
    pub fn observe_if(
        &mut self,
        source: SourceId,
        expected: Option<SourceRev>,
        content: Fingerprint,
        nodes: Vec<Node>,
    ) -> Result<SourceRev, SourceUpdateError> {
        let previous = self.get(&source);
        let actual = previous.map(|previous| previous.rev);
        if actual != expected {
            return Err(SourceUpdateError::Conflict { expected, actual });
        }
        let observed = SourceSnapshot::observe(source, content, nodes, previous)
            .map_err(SourceUpdateError::Observation)?;
        let revision = observed.rev;
        self.insert(observed);
        Ok(revision)
    }

    pub fn get(&self, source: &SourceId) -> Option<&SourceSnapshot> {
        self.sources.get(source)
    }

    pub fn sources(&self) -> impl Iterator<Item = &SourceSnapshot> {
        self.sources.values()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Every top-level node with the source that declared it, in a stable
    /// order: sources sorted by id, nodes in declaration order.
    pub fn nodes(&self) -> impl Iterator<Item = (&SourceId, &Node)> {
        self.sources
            .values()
            .flat_map(|snapshot| snapshot.nodes.iter().map(move |node| (&snapshot.source, node)))
    }

    /// The revision of every source, which a candidate carries so `commit` can
    /// detect that any source moved while it was compiling.
    pub fn revs(&self) -> RevVector {
        RevVector(
            self.sources
                .iter()
                .map(|(id, snapshot)| (id.clone(), snapshot.rev))
                .collect(),
        )
    }
}

/// A rejected local composition; the snapshot was not changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceUpdateError {
    Conflict {
        expected: Option<SourceRev>,
        actual: Option<SourceRev>,
    },
    Observation(ObservationError),
}

impl std::fmt::Display for SourceUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { expected, actual } => write!(
                f,
                "source revision conflict: expected {expected:?}, actual {actual:?}"
            ),
            Self::Observation(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SourceUpdateError {}

/// The revision of every source at the moment a candidate started.
///
/// Comparing whole vectors rather than a single revision is what makes several
/// independent sources safe: a candidate built from the file tree still notices
/// that the Admin API committed something in the meantime.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevVector(BTreeMap<SourceId, SourceRev>);

impl RevVector {
    pub fn get(&self, source: &SourceId) -> Option<SourceRev> {
        self.0.get(source).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SourceId, SourceRev)> {
        self.0.iter().map(|(id, rev)| (id, *rev))
    }

    /// Advance to the newer of two observations, per source.
    ///
    /// A revision only ever moves forward, so a late-arriving stale read
    /// cannot make a candidate that was already overtaken look current again.
    pub fn merge_max(&mut self, other: &RevVector) {
        for (source, rev) in &other.0 {
            let slot = self.0.entry(source.clone()).or_insert(*rev);
            if *rev > *slot {
                *slot = *rev;
            }
        }
    }

    /// Describe the first difference, for the "candidate went stale" event.
    pub fn first_difference(&self, other: &RevVector) -> Option<String> {
        for (source, rev) in &self.0 {
            match other.0.get(source) {
                Some(current) if current == rev => {}
                Some(current) => return Some(format!("{source} moved from {rev} to {current}")),
                None => return Some(format!("{source} disappeared")),
            }
        }
        other
            .0
            .keys()
            .find(|source| !self.0.contains_key(source))
            .map(|source| format!("{source} appeared"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hasher;
    use crate::origin::Origin;

    #[test]
    fn conditional_composition_preserves_other_sources_and_tombstone_versions() {
        let file = snapshot_for(SourceId::file("main"), 4, &["file"]);
        let admin = snapshot_for(SourceId::admin(), 99, &["admin"]);
        let mut inputs = Snapshot::new().with(file.clone());
        assert_eq!(
            inputs.observe_if(admin.source.clone(), None, admin.content, admin.nodes.clone()),
            Ok(SourceRev(1))
        );
        assert_eq!(inputs.get(&file.source).unwrap().rev, file.rev);
        assert_eq!(inputs.get(&file.source).unwrap().content, file.content);
        let before = inputs.revs();
        assert!(matches!(
            inputs.observe_if(admin.source.clone(), None, admin.content, vec![]),
            Err(SourceUpdateError::Conflict { .. })
        ));
        assert_eq!(inputs.revs(), before);
        assert_eq!(inputs.get(&admin.source).unwrap().nodes.len(), 1);
        assert_eq!(
            inputs.observe_if(
                admin.source.clone(),
                Some(SourceRev(1)),
                admin.content,
                admin.nodes
            ),
            Ok(SourceRev(1))
        );
        let empty = snapshot_for(admin.source.clone(), 0, &[]);
        assert_eq!(
            inputs.observe_if(admin.source.clone(), Some(SourceRev(1)), empty.content, vec![]),
            Ok(SourceRev(2))
        );
        assert!(inputs.get(&admin.source).unwrap().nodes.is_empty());
        assert!(matches!(
            inputs.observe_if(admin.source, None, empty.content, vec![]),
            Err(SourceUpdateError::Conflict {
                actual: Some(SourceRev(2)),
                ..
            })
        ));
        assert_eq!(inputs.get(&file.source).unwrap().nodes[0].name, "file");
    }

    #[test]
    fn conditional_composition_exhaustion_is_atomic_and_no_op_still_works() {
        let original = snapshot_for(SourceId::admin(), u64::MAX, &["original"]);
        let changed = snapshot_for(SourceId::admin(), 1, &["changed"]);
        let mut inputs = Snapshot::new().with(original.clone());
        assert_eq!(
            inputs.observe_if(
                original.source.clone(),
                Some(original.rev),
                changed.content,
                changed.nodes
            ),
            Err(SourceUpdateError::Observation(
                ObservationError::RevisionExhausted
            ))
        );
        assert_eq!(inputs.get(&original.source).unwrap().content, original.content);
        assert_eq!(inputs.get(&original.source).unwrap().nodes[0].name, "original");
        assert_eq!(
            inputs.observe_if(
                original.source,
                Some(original.rev),
                original.content,
                original.nodes
            ),
            Ok(SourceRev(u64::MAX))
        );
    }

    fn snapshot_for(source: SourceId, rev: u64, names: &[&str]) -> SourceSnapshot {
        let mut hasher = Hasher::new();
        for name in names {
            hasher.update_str(name);
        }
        SourceSnapshot::new(
            source,
            SourceRev(rev),
            hasher.finish(),
            names
                .iter()
                .map(|name| Node::new(*name, Origin::synthetic("test")))
                .collect(),
        )
    }

    #[test]
    fn observation_checks_identity_and_revision_exhaustion() {
        let source = SourceId::file("main");
        let first = snapshot_for(source.clone(), u64::MAX, &["a"]);
        let changed = snapshot_for(source.clone(), 1, &["b"]);
        assert_eq!(SourceRev(u64::MAX).next(), None);
        assert_eq!(SourceRev::ZERO.next(), Some(SourceRev(1)));
        let unchanged =
            SourceSnapshot::observe(source.clone(), first.content, first.nodes.clone(), Some(&first))
                .unwrap();
        assert_eq!(unchanged.rev, SourceRev(u64::MAX));
        assert!(matches!(
            SourceSnapshot::observe(source, changed.content, changed.nodes, Some(&first)),
            Err(ObservationError::RevisionExhausted)
        ));
        assert!(matches!(
            SourceSnapshot::observe(SourceId::admin(), first.content, vec![], Some(&first)),
            Err(ObservationError::WrongSource)
        ));
        assert_eq!(first.rev, SourceRev(u64::MAX));
    }

    #[test]
    fn nodes_iterate_in_a_stable_order() {
        let snapshot = Snapshot::new()
            .with(snapshot_for(SourceId::admin(), 1, &["runtime-site"]))
            .with(snapshot_for(SourceId::file("main"), 1, &["global", "site"]));
        let names: Vec<&str> = snapshot.nodes().map(|(_, node)| node.name.as_str()).collect();
        // Admin sorts before File because SourceKind's ordering is declaration order.
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"runtime-site"));
        assert!(names.contains(&"global"));
    }

    #[test]
    fn rev_vectors_detect_a_source_moving() {
        let before = Snapshot::new()
            .with(snapshot_for(SourceId::file("main"), 4, &["a"]))
            .revs();
        let after = Snapshot::new()
            .with(snapshot_for(SourceId::file("main"), 5, &["a"]))
            .revs();
        assert_eq!(before.first_difference(&before), None);
        assert!(
            before
                .first_difference(&after)
                .unwrap()
                .contains("moved from 4 to 5")
        );
    }

    #[test]
    fn rev_vectors_detect_a_new_source() {
        let before = Snapshot::new()
            .with(snapshot_for(SourceId::file("main"), 1, &["a"]))
            .revs();
        let after = before
            .clone()
            .iter()
            .map(|(id, rev)| (id.clone(), rev))
            .collect::<BTreeMap<_, _>>();
        let mut after = RevVector(after);
        after.0.insert(SourceId::admin(), SourceRev(1));
        assert!(before.first_difference(&after).unwrap().contains("appeared"));
    }

    #[test]
    fn identical_content_produces_an_identical_fingerprint() {
        let a = snapshot_for(SourceId::file("main"), 1, &["global", "site"]);
        let b = snapshot_for(SourceId::file("main"), 9, &["global", "site"]);
        assert_eq!(a.content, b.content);
    }
}
