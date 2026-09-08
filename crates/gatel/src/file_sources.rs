//! File frontends retain a complete intent union under one observation lock.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use gatel_model::{Diagnostics, Snapshot, SourceId};

#[derive(Debug, Clone)]
pub(crate) struct NamedFile {
    name: String,
    path: PathBuf,
}

impl FromStr for NamedFile {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, path) = value.split_once('=').ok_or("source must be NAME=PATH")?;
        if name.is_empty()
            || name.len() > 64
            || name == "main"
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("source name must be 1-64 ASCII letters/digits/-/_, and cannot be main".into());
        }
        if path.is_empty() {
            return Err("source path must not be empty".into());
        }
        Ok(Self {
            name: name.into(),
            path: path.into(),
        })
    }
}

pub(crate) struct FileSources {
    roots: BTreeMap<SourceId, PathBuf>,
    previous: Mutex<Snapshot>,
}

impl FileSources {
    pub fn new(root: &Path, extra: Vec<NamedFile>) -> Result<Self, String> {
        if extra.len() > 31 {
            return Err("at most 31 additional file sources are supported".into());
        }
        let mut roots = BTreeMap::from([(SourceId::file("main"), root.to_path_buf())]);
        for item in extra {
            if roots.insert(SourceId::file(&*item.name), item.path).is_some() {
                return Err(format!("duplicate source name: {}", item.name));
            }
        }
        Ok(Self {
            roots,
            previous: Mutex::default(),
        })
    }

    pub fn identities(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.roots.keys().cloned()
    }

    #[cfg(test)]
    pub fn observe(&self) -> Result<Snapshot, Diagnostics> {
        self.observe_from(&Snapshot::new())
    }

    pub fn observe_from(&self, admitted: &Snapshot) -> Result<Snapshot, Diagnostics> {
        let mut previous = self.previous.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = Snapshot::new();
        for (source, path) in &self.roots {
            next.insert(gatel_kdl::snapshot(
                path,
                source.clone(),
                newest(previous.get(source), admitted.get(source)),
            )?);
        }
        // No partial revision cache advancement if any root/import fails.
        *previous = next.clone();
        Ok(next)
    }

    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.roots
            .values()
            .flat_map(|path| match gatel_kdl::load(path) {
                Ok(loaded) => loaded.files,
                Err(_) => vec![path.clone()],
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// The host's recovered head can be newer than this process's file cache. Keep
/// a still newer local observation too (e.g. rejected before store admission).
/// On equal revisions the admitted source is authoritative.
pub(crate) fn newest<'a>(
    cached: Option<&'a gatel_model::SourceSnapshot>,
    admitted: Option<&'a gatel_model::SourceSnapshot>,
) -> Option<&'a gatel_model::SourceSnapshot> {
    match (cached, admitted) {
        (Some(cached), Some(admitted)) if cached.rev > admitted.rev => Some(cached),
        (_, Some(admitted)) => Some(admitted),
        (cached, None) => cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_selection_never_regresses_and_prefers_admitted_on_equal_revision() {
        let make = |rev| {
            gatel_model::SourceSnapshot::new(
                SourceId::file("main"),
                gatel_model::SourceRev(rev),
                gatel_model::Hasher::new().finish(),
                vec![],
            )
        };
        let older = make(4);
        let newer = make(8);
        let equal = make(8);
        assert!(std::ptr::eq(newest(Some(&older), Some(&newer)).unwrap(), &newer));
        assert!(std::ptr::eq(newest(Some(&newer), Some(&older)).unwrap(), &newer));
        assert!(std::ptr::eq(newest(Some(&newer), Some(&equal)).unwrap(), &equal));
        assert!(std::ptr::eq(newest(None, Some(&newer)).unwrap(), &newer));
        assert!(std::ptr::eq(newest(Some(&newer), None).unwrap(), &newer));
        assert!(newest(None, None).is_none());
    }

    #[test]
    fn failed_root_does_not_commit_partial_observation_revisions() {
        let directory = std::env::temp_dir().join(format!("gatel-file-sources-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let main = directory.join("main.kdl");
        let extra = directory.join("extra.kdl");
        std::fs::write(&main, "node \"first\"\n").unwrap();
        std::fs::write(&extra, "node \"extra\"\n").unwrap();
        let files = FileSources::new(&main, vec![format!("z={}", extra.display()).parse().unwrap()]).unwrap();
        let first = files.observe().unwrap();
        assert_eq!(first.sources().count(), 2);
        std::fs::write(&main, "node \"second\"\n").unwrap();
        std::fs::write(&extra, "node {\n").unwrap();
        assert!(files.observe().is_err());
        assert_eq!(files.previous.lock().unwrap().revs(), first.revs());
        std::fs::write(&extra, "node \"extra\"\n").unwrap();
        let next = files.observe().unwrap();
        assert_eq!(
            next.get(&SourceId::file("main")).unwrap().rev,
            gatel_model::SourceRev(2)
        );
        assert_eq!(
            next.get(&SourceId::file("z")).unwrap().rev,
            gatel_model::SourceRev(1)
        );
        assert_eq!(files.watched_paths().len(), 2);
        let mut recovered = Snapshot::new();
        for part in next.sources() {
            let mut part = part.clone();
            part.rev = gatel_model::SourceRev(50);
            recovered.insert(part);
        }
        std::fs::write(&main, "node \"third\"\n").unwrap();
        std::fs::write(&extra, "node {\n").unwrap();
        assert!(files.observe_from(&recovered).is_err());
        assert_eq!(files.previous.lock().unwrap().revs(), next.revs());
        std::fs::write(&extra, "node \"extra\"\n").unwrap();
        let resumed = files.observe_from(&recovered).unwrap();
        assert_eq!(
            resumed.get(&SourceId::file("main")).unwrap().rev,
            gatel_model::SourceRev(51)
        );
        assert_eq!(
            resumed.get(&SourceId::file("z")).unwrap().rev,
            gatel_model::SourceRev(50)
        );
        std::fs::remove_file(main).unwrap();
        std::fs::remove_file(extra).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn source_names_and_counts_are_bounded_and_never_silently_overwrite() {
        for invalid in ["x", "=file", "main=file", "a/b=file", "a=", " 名=file"] {
            assert!(invalid.parse::<NamedFile>().is_err(), "{invalid}");
        }
        assert!("a=path=with spaces.kdl".parse::<NamedFile>().is_ok());
        assert!(
            FileSources::new(
                Path::new("main.kdl"),
                vec!["a=a.kdl".parse().unwrap(), "a=b.kdl".parse().unwrap()]
            )
            .is_err()
        );
        assert!(
            FileSources::new(
                Path::new("main.kdl"),
                (0..32).map(|i| format!("s{i}=file").parse().unwrap()).collect()
            )
            .is_err()
        );
    }
}
