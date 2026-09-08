//! Real process termination at checkpoint boundaries, not a power-loss model.
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

// Both generations carry distinct, invalid latest intent as well as valid
// serving sources. A complete target must retain both halves after process kill.
fn committed(body: &str, registry: &Registry) -> ConfigurationCapture<()> {
    let first = super::committed(body, registry);
    let generation = GenerationStore::<()>::new();
    let mut observed = first.observed_snapshot().clone();
    generation.attempt(&observed, registry, |_| Ok(()));
    let text = format!("invalid-directive \"{body}-rejected-secret\";");
    let mut hash = Hasher::new();
    hash.update_str(&text);
    observed.insert(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        hash.finish(),
        gatel_kdl::lower(&text, "admin").unwrap(),
    ));
    assert!(matches!(
        generation.attempt(&observed, registry, |_| Ok(())),
        Outcome::Rejected(_)
    ));
    generation.capture_configuration()
}

fn mode(name: &str) -> (bool, Phase) {
    match name {
        "first-staged" => (false, Phase::Synced),
        "first-renamed" => (false, Phase::Renamed),
        "replace-staged" => (true, Phase::Synced),
        "replace-renamed" => (true, Phase::Renamed),
        "replace-directory-synced" => (true, Phase::DirectorySynced),
        _ => panic!("invalid checkpoint crash fixture mode"),
    }
}

#[test]
fn killed_checkpoint_writer_retains_complete_target_and_ignores_staged_files() {
    const DIRECTORY: &str = "GATEL_CHECKPOINT_CRASH_DIRECTORY";
    const MODE: &str = "GATEL_CHECKPOINT_CRASH_MODE";
    if let Some(path) = std::env::var_os(DIRECTORY) {
        let directory = Path::new(&path);
        let (replacing, cut) = mode(&std::env::var(MODE).unwrap());
        let registry = registry();
        let mut state = State::default();
        eprintln!("crash fixture: opening checkpoint journal");
        let mut store = binding::open(directory, INSTANCE, |seq, text| state.accept(seq, text)).unwrap();
        if replacing {
            eprintln!("crash fixture: saving old checkpoint");
            store
                .save_checkpoint(INSTANCE, &registry, &committed("old", &registry))
                .unwrap();
        }
        // The old checkpoint references sequence 1; the new one references 3.
        eprintln!("crash fixture: appending intent");
        store.append("GATEL-TX1 I\n").unwrap();
        eprintln!("crash fixture: appending decision");
        store.append("GATEL-TX1 A\n2").unwrap();
        eprintln!("crash fixture: saving new checkpoint");
        store
            .save_checkpoint_with(INSTANCE, &registry, &committed("new", &registry), |phase| {
                eprintln!("crash fixture: checkpoint phase {phase:?}");
                if phase == cut {
                    std::fs::write(directory.join("ready"), b"checkpoint-writer-paused").unwrap();
                    loop {
                        std::thread::park();
                    }
                }
                Ok(())
            })
            .unwrap();
        panic!("checkpoint crash fixture did not reach requested phase");
    }

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let registry = registry();
    let old = committed("old", &registry);
    let new = committed("new", &registry);
    for name in [
        "first-staged",
        "first-renamed",
        "replace-staged",
        "replace-renamed",
        "replace-directory-synced",
    ] {
        let (replacing, cut) = mode(name);
        let directory = Directory::new();
        let output = File::create(directory.0.join("child.log")).unwrap();
        let mut child = Process(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "admin::journal::store::checkpoint::tests::crash::killed_checkpoint_writer_retains_complete_target_and_ignores_staged_files", "--nocapture"])
            .env(DIRECTORY, &directory.0).env(MODE, name)
            .stdin(Stdio::null()).stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output))
            .spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.0.join("ready").exists() {
            let status = child.0.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < deadline,
                "checkpoint child mode={name} pid={} status={status:?}: {}",
                child.0.id(),
                std::fs::read_to_string(directory.0.join("child.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = match Journal::open(&directory.0, INSTANCE) {
            Ok(_) => panic!("competing writer acquired live child lock"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot acquire journal writer lock"));
        let log_before = std::fs::read(directory.0.join("intent.log")).unwrap();
        child.0.kill().unwrap();
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9));
        }

        let staged = std::fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(staged.len(), usize::from(cut == Phase::Synced));
        let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
        let recovered = journal.load_checkpoint(&registry).unwrap();
        if cut == Phase::Synced && !replacing {
            assert!(recovered.is_none(), "unpublished staging file was adopted");
        } else {
            let recovered = recovered.unwrap();
            let replaced = cut != Phase::Synced;
            assert_eq!(
                recovered.metadata.plan,
                if replaced {
                    new.committed().unwrap().plan_hash()
                } else {
                    old.committed().unwrap().plan_hash()
                }
            );
            assert_eq!(recovered.metadata.journal_sequence, if replaced { 3 } else { 1 });
            let expected = if replaced { &new } else { &old };
            assert_eq!(
                gatel_model::snapshot_codec::encode(&recovered.observed).unwrap(),
                gatel_model::snapshot_codec::encode(expected.observed_snapshot()).unwrap(),
            );
            assert!(gatel_plan::normalize(&recovered.observed, &registry).is_err());
            assert!(gatel_plan::normalize(&recovered.sources, &registry).is_ok());
        }
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log_before);
        // A later successful save must neither adopt nor delete the orphan.
        journal.save_checkpoint(&registry, &new).unwrap();
        assert_eq!(
            journal.load_checkpoint(&registry).unwrap().unwrap().metadata.plan,
            new.committed().unwrap().plan_hash()
        );
        for (path, bytes) in staged {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        drop(journal);
        let mut reopened = Journal::open(&directory.0, INSTANCE).unwrap();
        assert_eq!(
            reopened
                .load_checkpoint(&registry)
                .unwrap()
                .unwrap()
                .metadata
                .journal_sequence,
            3
        );
    }
}
