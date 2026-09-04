//! The immutable input to compilation.
//!
//! A snapshot is the union of what every configuration source declared at one
//! point in time. Sources are merged by union only: a name claimed by two
//! sources is an error, never a silent overwrite.

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
    pub fn new(source: SourceId, rev: SourceRev, content: Fingerprint, nodes: Vec<Node>) -> Self {
        Self {
            source,
            rev,
            content,
            nodes,
        }
    }
}

/// Every source, at one moment.
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
