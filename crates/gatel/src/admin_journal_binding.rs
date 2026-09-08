//! Mandatory first-record instance binding, independent of per-boot versions.
//! Trusted hosts choose a stable identity; this is accidental reuse protection,
//! not authentication against a writer that can replace or forge the log.
use std::io;
use std::path::Path;

use super::store::Store;
use super::{Error, checksum};

const PREFIX: &str = "GATEL-INSTANCE1\n";

/// `instance` must be stable across restarts and different for independent
/// deployments. Never use BootId, a bearer secret or an inferred working dir.
pub(super) fn open(
    directory: &Path,
    instance: &str,
    visit: impl FnMut(u64, &str) -> Result<(), Error>,
) -> io::Result<Store> {
    open_mode(directory, instance, visit, true)
}

pub(super) fn open_existing(
    directory: &Path,
    instance: &str,
    visit: impl FnMut(u64, &str) -> Result<(), Error>,
) -> io::Result<Store> {
    open_mode(directory, instance, visit, false)
}

fn open_mode(
    directory: &Path,
    instance: &str,
    mut visit: impl FnMut(u64, &str) -> Result<(), Error>,
    create: bool,
) -> io::Result<Store> {
    let expected = record(instance)?; // Validate before creating any files.
    let replay = |sequence, payload: &str| {
        if sequence == 1 {
            if payload != expected {
                return Err(Error::InvalidBinding);
            }
            return Ok(());
        }
        if payload.starts_with(PREFIX) {
            return Err(Error::InvalidBinding);
        }
        visit(sequence, payload)
    };
    let mut store = if create {
        Store::open_replaying(directory, replay)?
    } else {
        Store::open_existing_replaying(directory, replay)?
    };
    let recovered = store.recovered();
    if recovered.sequence == 0 {
        if recovered.incomplete_tail || !create {
            // Identity was never completely read. Do not assign the caller's
            // identity to a pre-existing partial log or truncate it on open.
            return Err(io::Error::new(io::ErrorKind::InvalidData, Error::InvalidBinding));
        }
        store.append(&expected)?;
    }
    Ok(store)
}

pub(super) fn record(instance: &str) -> io::Result<String> {
    if instance.is_empty() || instance.len() > 256 || !instance.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal instance identity must be 1..256 printable ASCII bytes without whitespace",
        ));
    }
    Ok(format!(
        "{PREFIX}{}",
        checksum("gatel.admin-journal.instance.v1", instance.as_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::journal::encode;
    use crate::admin::journal::transaction::{Decision, Journal};

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("gatel-binding-{}", gatel_model::BootId::generate()));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            builder.create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stable_identity_reopens_and_mismatch_does_not_replay_or_rewrite() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "deployment-a").unwrap();
        let intent = journal.begin("private-config;").unwrap();
        assert_eq!(intent, 2, "first sequence belongs to instance binding");
        journal.finish(intent, Decision::Applied).unwrap();
        drop(journal);
        let path = directory.0.join("intent.log");
        let before = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&before).contains("deployment-a"));
        let mut visits = 0;
        let error = match open(&directory.0, "deployment-b", |_, _| {
            visits += 1;
            Ok(())
        }) {
            Ok(_) => panic!("foreign journal opened"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("instance binding"));
        assert_eq!(visits, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let journal = Journal::open(&directory.0, "deployment-a").unwrap();
        assert_eq!(journal.state().applied.as_ref().unwrap().text, "private-config;");
    }

    #[test]
    fn invalid_identity_has_no_filesystem_side_effects() {
        let directory = Directory::new();
        for identity in [
            "".to_owned(),
            "has space".to_owned(),
            "line\nbreak".to_owned(),
            "配置".to_owned(),
            "x".repeat(257),
        ] {
            assert!(Journal::open(&directory.0, &identity).is_err());
            assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        }
        drop(Journal::open(&directory.0, &"x".repeat(256)).unwrap());
    }

    #[test]
    fn partial_binding_is_never_claimed_or_automatically_repaired() {
        let directory = Directory::new();
        // Start through Store to ensure private file permissions on Unix.
        drop(Store::open(&directory.0).unwrap());
        let path = directory.0.join("intent.log");
        let binding = encode(1, &record("deployment-a").unwrap()).unwrap();
        for cut in 1..binding.len() {
            std::fs::write(&path, &binding[..cut]).unwrap();
            for instance in ["deployment-a", "deployment-b"] {
                assert!(Journal::open(&directory.0, instance).is_err(), "cut={cut}");
                assert_eq!(std::fs::read(&path).unwrap(), binding[..cut]);
            }
        }
        std::fs::write(&path, &binding).unwrap();
        let journal = Journal::open(&directory.0, "deployment-a").unwrap();
        assert!(journal.state().latest.is_none());
    }

    #[test]
    fn killed_writer_preserves_binding_and_each_transaction_outcome() {
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};
        const DIRECTORY: &str = "GATEL_BOUND_JOURNAL_CRASH_DIRECTORY";
        const MODE: &str = "GATEL_BOUND_JOURNAL_CRASH_MODE";
        const INSTANCE: &str = "crash-fixture-instance";
        if let Some(path) = std::env::var_os(DIRECTORY) {
            let directory = Path::new(&path);
            eprintln!("crash fixture: opening bound journal");
            let mut journal = Journal::open(directory, INSTANCE).unwrap();
            eprintln!("crash fixture: beginning confirmed intent");
            let first = journal.begin("confirmed;").unwrap();
            eprintln!("crash fixture: applying confirmed intent");
            journal.finish(first, Decision::Applied).unwrap();
            eprintln!("crash fixture: beginning next intent");
            let next = journal.begin("next;").unwrap();
            eprintln!("crash fixture: finishing next intent");
            match std::env::var(MODE).unwrap().as_str() {
                "applied" => {
                    journal.finish(next, Decision::Applied).unwrap();
                }
                "rejected" => {
                    journal.finish(next, Decision::Rejected).unwrap();
                }
                "unknown" => {}
                _ => panic!("unknown fixture mode"),
            }
            eprintln!("crash fixture: signaling ready");
            std::fs::write(directory.join("ready"), b"synced-and-holding-lock").unwrap();
            loop {
                std::thread::park();
            }
        }
        struct Process(Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for mode in ["applied", "rejected", "unknown"] {
            let directory = Directory::new();
            let output = std::fs::File::create(directory.0.join("child.log")).unwrap();
            let mut child = Process(Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "admin::journal::binding::tests::killed_writer_preserves_binding_and_each_transaction_outcome", "--nocapture"])
                .env(DIRECTORY, &directory.0).env(MODE, mode)
                .stdin(Stdio::null()).stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output))
                .spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(10);
            while !directory.0.join("ready").exists() {
                let status = child.0.try_wait().unwrap();
                assert!(
                    status.is_none() && Instant::now() < deadline,
                    "bound journal child mode={mode} pid={} status={status:?}: {}",
                    child.0.id(),
                    std::fs::read_to_string(directory.0.join("child.log")).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(Journal::open(&directory.0, INSTANCE).is_err());
            child.0.kill().unwrap();
            let status = child.0.wait().unwrap();
            assert!(!status.success());
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(9));
            }
            let path = directory.0.join("intent.log");
            let before = std::fs::read(&path).unwrap();
            assert!(Journal::open(&directory.0, "wrong-instance").is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
            let state = journal.state();
            assert_eq!(state.latest.as_ref().unwrap().text, "next;");
            assert_eq!(
                state.applied.as_ref().unwrap().text,
                if mode == "applied" { "next;" } else { "confirmed;" }
            );
            assert_eq!(
                state.decision,
                match mode {
                    "applied" => Some(Decision::Applied),
                    "rejected" => Some(Decision::Rejected),
                    _ => None,
                }
            );
            assert_eq!(journal.begin("after-restart;").is_ok(), mode != "unknown");
        }
    }

    #[test]
    fn unbound_duplicate_or_misplaced_binding_records_fail_closed() {
        let directory = Directory::new();
        drop(Store::open(&directory.0).unwrap());
        let path = directory.0.join("intent.log");
        let binding = record("deployment-a").unwrap();
        let mut duplicate = encode(1, &binding).unwrap();
        duplicate.extend(encode(2, &binding).unwrap());
        for bytes in [
            encode(1, "GATEL-TX1 I\nlegacy;").unwrap(),
            encode(1, "GATEL-INSTANCE2\nunknown-version").unwrap(),
            duplicate,
        ] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(Journal::open(&directory.0, "deployment-a").is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}
