//! The KDL file frontend.
//!
//! Reads a configuration tree from disk and produces a
//! [`gatel_model::SourceSnapshot`]. Nothing downstream knows KDL exists.

#![forbid(unsafe_code)]

pub mod load;
pub mod lower;

use std::path::Path;

use gatel_model::{Diagnostics, SourceId, SourceRev, SourceSnapshot};
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
    let rev = match previous {
        Some(previous) if previous.content == loaded.content => previous.rev,
        Some(previous) => previous.rev.next(),
        None => SourceRev(1),
    };
    Ok(SourceSnapshot::new(source, rev, loaded.content, loaded.nodes))
}

#[cfg(test)]
mod tests {
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
