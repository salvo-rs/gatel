//! Kill owned child processes at publication boundaries; not a power-loss test.
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gatel_model::snapshot_codec;

use super::super::compact::{Phase, publish_with};
use super::*;

fn phase(name: &str) -> Phase {
    match name {
        "log" => Phase::LogSynced,
        "checkpoint" => Phase::CheckpointSynced,
        "before-publish" => Phase::BeforePublish,
        "published" => Phase::Published,
        _ => panic!("invalid compaction crash phase"),
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_compaction_keeps_source_and_only_publishes_complete_destination() {
    const DIRECTORY: &str = "GATEL_COMPACTION_CRASH_DIRECTORY";
    const MODE: &str = "GATEL_COMPACTION_CRASH_MODE";
    if let Some(path) = std::env::var_os(DIRECTORY) {
        let directory = Path::new(&path);
        let registry = registry();
        // Keep the real source writer lock for the entire export, as the CLI does.
        let mut source = Journal::open_existing(directory, INSTANCE).unwrap();
        assert!(source.load_checkpoint(&registry).unwrap().is_some());
        let original = std::fs::read(directory.join("checkpoint.bin")).unwrap();
        let log = std::fs::read(directory.join("intent.log")).unwrap();
        let (checkpoint, log) = checkpoint::compact(&original, INSTANCE, &registry, &log).unwrap();
        let stop = phase(&std::env::var(MODE).unwrap());
        publish_with(&directory.join("compacted"), &checkpoint, &log, |current| {
            if current == stop {
                std::fs::write(directory.join("ready"), b"compactor-paused").unwrap();
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        })
        .unwrap();
        panic!("compaction crash phase was not reached");
    }

    let registry = registry();
    for name in ["log", "checkpoint", "before-publish", "published"] {
        let directory = Directory::new();
        let initial = committed("serving-secret", &registry);
        let generations = GenerationStore::<()>::new();
        let mut observed = initial.observed_snapshot().clone();
        generations.attempt(&observed, &registry, |_| Ok(()));
        observed.insert(SourceSnapshot::new(
            SourceId::admin(),
            SourceRev(1),
            Hasher::new().finish(),
            gatel_kdl::lower("invalid-directive \"rejected-secret\";", "admin").unwrap(),
        ));
        assert!(matches!(
            generations.attempt(&observed, &registry, |_| Ok(())),
            Outcome::Rejected(_)
        ));
        let mut source = Journal::open(&directory.0, INSTANCE).unwrap();
        let intent = source.begin("old-history;").unwrap();
        source.finish(intent, Decision::Rejected).unwrap();
        source
            .save_checkpoint(&registry, &generations.capture_configuration())
            .unwrap();
        let expected = source.load_checkpoint(&registry).unwrap().unwrap();
        drop(source);
        let original = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
        let original_log = std::fs::read(directory.0.join("intent.log")).unwrap();
        let output = File::create(directory.0.join("child.log")).unwrap();
        let mut child = Process(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "admin::journal::store::checkpoint::tests::compaction_crash::killed_compaction_keeps_source_and_only_publishes_complete_destination", "--nocapture"])
            .env(DIRECTORY, &directory.0).env(MODE, name)
            .stdin(Stdio::null()).stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output))
            .spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.0.join("ready").exists() {
            let status = child.0.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < deadline,
                "compactor phase={name} status={status:?}: {}",
                std::fs::read_to_string(directory.0.join("child.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Journal::open_existing(&directory.0, INSTANCE).is_err(),
            "source lock lost during export"
        );
        child.0.kill().unwrap();
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9));
        }
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(directory.0.join("intent.log")).unwrap(),
            original_log
        );
        let destination = directory.0.join("compacted");
        assert_eq!(destination.exists(), phase(name) == Phase::Published);
        let orphans = std::fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".gatel-compact-")
            })
            .collect::<Vec<_>>();
        assert_eq!(orphans.len(), usize::from(phase(name) != Phase::Published));
        let orphan_files = orphans
            .iter()
            .flat_map(|path| std::fs::read_dir(path).unwrap())
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect::<Vec<_>>();
        let mut source = Journal::open_existing(&directory.0, INSTANCE).unwrap();
        assert_eq!(
            source.load_checkpoint(&registry).unwrap().unwrap().metadata,
            expected.metadata
        );
        if !destination.exists() {
            source.compact_to(&registry, &destination).unwrap();
        } else {
            assert!(
                source.compact_to(&registry, &destination).is_err(),
                "published target overwritten"
            );
        }
        let mut restored = Journal::open_existing(&destination, INSTANCE).unwrap();
        let checkpoint = restored.load_checkpoint(&registry).unwrap().unwrap();
        assert_eq!(checkpoint.metadata.journal_sequence, 1);
        assert_eq!(checkpoint.metadata.publication, expected.metadata.publication);
        assert_eq!(
            snapshot_codec::encode(&checkpoint.sources).unwrap(),
            snapshot_codec::encode(&expected.sources).unwrap()
        );
        assert_eq!(
            snapshot_codec::encode(&checkpoint.observed).unwrap(),
            snapshot_codec::encode(&expected.observed).unwrap()
        );
        assert!(gatel_plan::normalize(&checkpoint.observed, &registry).is_err());
        for (path, bytes) in orphan_files {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(directory.0.join("intent.log")).unwrap(),
            original_log
        );
    }
}
