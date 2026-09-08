//! Reconcile subscriptions without recording failed operations as successful.
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct Directories {
    active: BTreeSet<PathBuf>,
}

impl Directories {
    #[cfg(test)]
    fn refresh(&mut self, files: Vec<PathBuf>, mut apply: impl FnMut(&std::path::Path, bool) -> bool) {
        self.reconcile(Self::parents(files), &mut apply);
    }

    pub fn parents(files: Vec<PathBuf>) -> BTreeSet<PathBuf> {
        files
            .iter()
            .filter_map(|file| file.parent())
            .map(|path| {
                if path.as_os_str().is_empty() {
                    std::path::Path::new(".")
                } else {
                    path
                }
            })
            .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
            .collect()
    }

    pub fn reconcile(
        &mut self,
        desired: BTreeSet<PathBuf>,
        mut apply: impl FnMut(&std::path::Path, bool) -> bool,
    ) {
        // Add first, so moving an import never removes its old subscription
        // before attempting to watch its replacement. Polling remains required.
        for directory in desired.difference(&self.active).cloned().collect::<Vec<_>>() {
            if apply(&directory, true) {
                self.active.insert(directory);
            }
        }
        for directory in self.active.difference(&desired).cloned().collect::<Vec<_>>() {
            if apply(&directory, false) {
                self.active.remove(&directory);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_and_reconciles_additions_before_removals() {
        let mut directories = Directories::default();
        let files = vec![
            PathBuf::from("watch-fixture-a/root"),
            PathBuf::from("watch-fixture-a/import"),
        ];
        let mut operations = Vec::new();
        directories.refresh(files.clone(), |path, add| {
            operations.push((path.to_path_buf(), add));
            true
        });
        assert_eq!(operations.len(), 1);
        directories.refresh(files, |_, _| panic!("unchanged subscription"));
        operations.clear();
        directories.refresh(vec![PathBuf::from("watch-fixture-b/root")], |path, add| {
            operations.push((path.to_path_buf(), add));
            true
        });
        assert_eq!(operations.len(), 2);
        assert!(operations[0].1);
        assert!(!operations[1].1);
    }

    #[test]
    fn failures_are_retried_and_successes_are_not() {
        let mut directories = Directories::default();
        let files = vec![PathBuf::from("watch-fixture-c/root")];
        directories.refresh(files.clone(), |_, add| {
            assert!(add);
            false
        });
        assert!(directories.active.is_empty());
        directories.refresh(files, |_, add| {
            assert!(add);
            true
        });
        assert_eq!(directories.active.len(), 1);
        directories.refresh(vec![], |_, add| {
            assert!(!add);
            false
        });
        assert_eq!(directories.active.len(), 1);
        directories.refresh(vec![], |_, add| {
            assert!(!add);
            true
        });
        assert!(directories.active.is_empty());
        directories.refresh(vec![], |_, _| panic!("already removed"));
    }
}
