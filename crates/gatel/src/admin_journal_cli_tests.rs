use super::*;
use crate::admin::journal::encode;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gatel-inspect-{}", gatel_model::BootId::generate()));
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }

    fn options(&self) -> Options {
        Options {
            directory: self.0.clone(),
            instance: "inspection-test".into(),
        }
    }

    fn files(&self) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn inspection_never_initializes_missing_or_unbound_storage() {
    let directory = Directory::new();
    let registry = crate::plugins::registry().unwrap();
    let mut options = directory.options();
    options.directory = directory.0.join("missing");
    assert!(inspect(&options, &registry).is_err());
    assert!(!options.directory.exists());
    options.directory = directory.0.clone();
    assert!(inspect(&options, &registry).is_err());
    assert!(directory.files().is_empty());
    // Create the empty files through the ordinary writer, but never bind them.
    drop(super::super::store::Store::open(&directory.0).unwrap());
    let before = directory.files();
    assert!(inspect(&options, &registry).is_err());
    assert_eq!(directory.files(), before);
    std::fs::remove_file(directory.0.join("intent.log")).unwrap();
    let before = directory.files();
    assert!(inspect(&options, &registry).is_err());
    assert_eq!(directory.files(), before);
}

#[test]
fn inspection_preserves_unknown_intent_and_partial_tail_and_redacts_content() {
    let directory = Directory::new();
    let options = directory.options();
    let registry = crate::plugins::registry().unwrap();
    let mut journal = Journal::open(&directory.0, &options.instance).unwrap();
    let intent = journal.begin("private-secret-kdl").unwrap();
    drop(journal);
    let before = directory.files();
    let (report, code) = inspect(&options, &registry).unwrap();
    assert_eq!(code, 1);
    assert!(report.starts_with("state=reconcile-unknown-outcome\n"));
    assert!(report.contains("checkpoint_serving_sources=none\n"));
    assert!(report.contains("checkpoint_observed_sources=none\n"));
    assert!(report.contains("checkpoint_empty_observed_sources=none\n"));
    assert!(report.contains("checkpoint_observed_differs=none\n"));
    assert!(!report.contains("private-secret"));
    assert_eq!(directory.files(), before);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(directory.0.join("intent.log"))
        .unwrap();
    let decision = encode(intent + 1, &format!("GATEL-TX1 A\n{intent}")).unwrap();
    file.write_all(&decision[..decision.len() - 1]).unwrap();
    drop(file);
    let before = directory.files();
    let (report, code) = inspect(&options, &registry).unwrap();
    assert_eq!(code, 1);
    assert!(report.starts_with("state=reconcile-incomplete-tail\n"));
    assert!(report.contains("latest_decision=none\n"));
    assert_eq!(directory.files(), before);
}

#[test]
fn inspection_requires_exclusive_lock_and_matching_identity() {
    let directory = Directory::new();
    let mut options = directory.options();
    let registry = crate::plugins::registry().unwrap();
    drop(Journal::open(&directory.0, &options.instance).unwrap());
    let before = directory.files();
    let journal = Journal::open(&directory.0, &options.instance).unwrap();
    assert!(inspect(&options, &registry).is_err());
    drop(journal);
    options.instance = "wrong".into();
    assert!(inspect(&options, &registry).is_err());
    assert_eq!(directory.files(), before);
    options.instance = "inspection-test".into();
    assert_eq!(
        inspect(&options, &registry).unwrap().0.split('\n').next(),
        Some("state=uninitialized")
    );
    assert_eq!(directory.files(), before);
}
