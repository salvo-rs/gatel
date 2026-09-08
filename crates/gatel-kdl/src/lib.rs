//! The KDL file frontend.
//!
//! Reads a configuration tree from disk and produces a
//! [`gatel_model::SourceSnapshot`]. Nothing downstream knows KDL exists.

#![forbid(unsafe_code)]

mod format;
pub mod load;
pub mod lower;
use std::path::Path;

pub use format::{Formatted, format_file};
use gatel_model::{Code, Diagnostics, Origin, SourceId, SourceSnapshot};
pub use load::{Limits, Loaded, load, load_with_limits};
pub use lower::{LineIndex, lower};

/// Read a configuration tree and package it as a snapshot.
///
/// `previous` is the last snapshot from the same source, if any. When the raw
/// bytes are unchanged the revision is carried over, so an editor writing a
/// file without changing it cannot advance the revision and invalidate other
/// sources' in-flight candidates.
pub fn snapshot(
    root: &Path,
    source: SourceId,
    previous: Option<&SourceSnapshot>,
) -> Result<SourceSnapshot, Diagnostics> {
    snapshot_with_limits(root, source, previous, Limits::default())
}

pub fn snapshot_with_limits(
    root: &Path,
    source: SourceId,
    previous: Option<&SourceSnapshot>,
    limits: Limits,
) -> Result<SourceSnapshot, Diagnostics> {
    let loaded = load_with_limits(root, limits)?;
    SourceSnapshot::observe(source, loaded.content, loaded.nodes, previous).map_err(|error| {
        let mut diagnostics = Diagnostics::new();
        diagnostics.error(
            Code::InvalidValue,
            Origin::synthetic("source observation"),
            error.to_string(),
        );
        diagnostics
    })
}

#[cfg(test)]
mod tests {
    use gatel_model::SourceRev;

    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gatel-kdl-lib-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn wrong_source_and_exhausted_revision_are_diagnostics_not_panics() {
        let dir = temp("exhausted");
        let root = dir.join("gatel.kdl");
        std::fs::write(&root, "site \"a\"\n").unwrap();
        let mut first = snapshot(&root, SourceId::file("main"), None).unwrap();
        let error = snapshot(&root, SourceId::admin(), Some(&first)).unwrap_err();
        assert!(error.iter().any(|d| d.message.contains("another source")));
        first.rev = SourceRev(u64::MAX);
        assert_eq!(
            snapshot(&root, first.source.clone(), Some(&first)).unwrap().rev,
            first.rev
        );
        std::fs::write(&root, "site \"b\"\n").unwrap();
        let error = snapshot(&root, first.source.clone(), Some(&first)).unwrap_err();
        assert!(
            error
                .iter()
                .any(|d| d.message == "source revision counter exhausted")
        );
        assert_eq!(first.rev, SourceRev(u64::MAX));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_file_keeps_its_revision() {
        let dir = temp("rev");
        let root = dir.join("gatel.kdl");
        std::fs::write(&root, "site \"a\"\n").unwrap();

        let first = snapshot(&root, SourceId::file("main"), None).unwrap();
        assert_eq!(first.rev, SourceRev(1));

        let second = snapshot(&root, SourceId::file("main"), Some(&first)).unwrap();
        assert_eq!(
            second.rev,
            SourceRev(1),
            "rewriting identical bytes must not advance the revision"
        );

        std::fs::write(&root, "site \"b\"\n").unwrap();
        let third = snapshot(&root, SourceId::file("main"), Some(&second)).unwrap();
        assert_eq!(third.rev, SourceRev(2));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
