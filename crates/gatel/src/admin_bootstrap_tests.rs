use clap::Parser;

use super::*;

#[path = "admin_bootstrap_process_tests.rs"]
mod process;

fn options(directory: &Path, instance: &str) -> crate::admin::bootstrap::Options {
    let cli = crate::Cli::try_parse_from([
        "gatel",
        "run",
        "--bootstrap-journal",
        directory.to_str().unwrap(),
        "--bootstrap-instance",
        instance,
    ])
    .unwrap();
    let crate::Command::Run { bootstrap, .. } = cli.command else {
        panic!("run command")
    };
    bootstrap
}

#[test]
fn bootstrap_requires_paired_options_and_refuses_missing_or_uncovered_storage() {
    assert!(crate::Cli::try_parse_from(["gatel", "run", "--bootstrap-journal", "missing"]).is_err());
    assert!(crate::Cli::try_parse_from(["gatel", "run", "--bootstrap-instance", "fixture"]).is_err());
    let directory = Directory::new();
    let registry = registry();
    assert!(options(&directory.0, INSTANCE).load(&registry).is_err());
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal
        .save_checkpoint(&registry, &committed("first", &registry))
        .unwrap();
    drop(journal);
    let loaded = options(&directory.0, INSTANCE).load(&registry).unwrap().unwrap();
    assert!(Journal::open(&directory.0, INSTANCE).is_err());
    drop(loaded);
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    let intent = journal.begin("invalid;").unwrap();
    journal.finish(intent, Decision::Rejected).unwrap();
    drop(journal);
    let before = std::fs::read(directory.0.join("intent.log")).unwrap();
    assert!(options(&directory.0, INSTANCE).load(&registry).is_err());
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), before);
}

#[test]
fn run_dry_run_bootstraps_checkpoint_without_reading_missing_root_or_writing_storage() {
    let directory = Directory::new();
    let registry = crate::plugins::registry().unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let text = format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"bootstrapped\"; }} }}"
    );
    let mut hash = Hasher::new();
    hash.update_str(&text);
    let sources = Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(17),
        hash.finish(),
        gatel_kdl::lower(&text, "fixture").unwrap(),
    ));
    let generation = GenerationStore::<()>::new();
    assert!(matches!(
        generation.attempt(&sources, &registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal
        .save_checkpoint(&registry, &generation.capture_configuration())
        .unwrap();
    drop(journal);
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    drop(socket);
    let code = crate::run(
        directory.0.join("missing.kdl"),
        true,
        vec![],
        crate::admin::cli::Options::default(),
        options(&directory.0, INSTANCE),
    );
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(!directory.0.join("missing.kdl").exists());
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert!(Journal::open(&directory.0, INSTANCE).is_ok());
    assert!(std::net::TcpListener::bind(address).is_ok());
}
