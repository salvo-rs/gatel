//! Exercise the feature-gated shipped command, including static error output.
use std::process::Command;

#[path = "support/cli_process.rs"]
mod process_output;
use process_output as cli_process;

#[test]
fn command_availability_follows_build_features() {
    let output =
        cli_process::output(Command::new(env!("CARGO_BIN_EXE_gatel")).args(["journal-inspect", "--help"]));
    assert_eq!(
        output.status.code(),
        Some(if cfg!(feature = "admin-api") { 0 } else { 2 })
    );
}

#[cfg(feature = "admin-api")]
#[test]
fn run_refuses_missing_bootstrap_before_opening_admin_credentials() {
    let directory = std::env::temp_dir().join(format!(
        "gatel-missing-bootstrap-{}",
        gatel_model::BootId::generate()
    ));
    let output = cli_process::output(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .args(["run", "missing-config.kdl", "--dry-run", "--bootstrap-journal"])
            .arg(&directory)
            .args([
                "--bootstrap-instance",
                "fixture",
                "--admin-listen",
                "127.0.0.1:0",
                "--admin-token-file",
                "missing-token-file",
            ]),
    );
    assert_eq!(output.status.code(), Some(3));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("cannot open bootstrap journal"), "{error}");
    assert!(!directory.exists());
}

#[cfg(feature = "admin-api")]
mod enabled {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("gatel-inspect-cli-{}", gatel_model::BootId::generate()));
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.0.join(name), bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(self.0.join(name), fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn hash(domain: &str, bytes: &[u8]) -> gatel_model::Fingerprint {
        let mut hash = gatel_model::Hasher::new();
        hash.update_str(domain);
        hash.update_bytes(bytes);
        hash.finish()
    }
    fn frame(sequence: u64, text: &str) -> Vec<u8> {
        let mut bytes = b"GATELJ01".to_vec();
        bytes.extend(sequence.to_le_bytes());
        bytes.extend((text.len() as u32).to_le_bytes());
        bytes.extend(hash("gatel.admin-intent.payload.v1", text.as_bytes()).as_bytes());
        bytes.extend(hash("gatel.admin-intent.header.v1", &bytes).as_bytes());
        bytes.extend(text.as_bytes());
        bytes
    }
    fn invoke(path: &Path, instance: &str) -> std::process::Output {
        cli_process::output(
            Command::new(env!("CARGO_BIN_EXE_gatel"))
                .arg("journal-inspect")
                .arg("--directory")
                .arg(path)
                .arg("--instance")
                .arg(instance)
                .env("GATEL_LOG", "trace"),
        )
    }

    #[test]
    fn existing_storage_only_reports_recovery_without_rewriting_or_leaking() {
        let directory = Directory::new();
        let missing = directory.0.join("missing-private-path");
        let failed = invoke(&missing, "private-instance");
        assert_eq!(failed.status.code(), Some(3));
        assert!(failed.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&failed.stderr).contains("private"));
        assert!(!missing.exists());
        directory.write("writer.lock", b"");
        let binding = format!(
            "GATEL-INSTANCE1\n{}",
            hash("gatel.admin-journal.instance.v1", b"fixture")
        );
        let mut log = frame(1, &binding);
        directory.write("intent.log", &log);
        let fresh = invoke(&directory.0, "fixture");
        assert_eq!(fresh.status.code(), Some(1), "{fresh:?}");
        assert!(fresh.stdout.starts_with(b"state=uninitialized\n"));
        assert!(fresh.stderr.is_empty());
        log.extend(frame(2, "GATEL-TX1 I\nsecret-config-body"));
        directory.write("intent.log", &log);
        let unknown = invoke(&directory.0, "fixture");
        assert_eq!(unknown.status.code(), Some(1));
        assert!(unknown.stdout.starts_with(b"state=reconcile-unknown-outcome\n"));
        assert!(!String::from_utf8_lossy(&unknown.stdout).contains("secret"));
        assert!(unknown.stderr.is_empty());
        assert_eq!(fs::read(directory.0.join("intent.log")).unwrap(), log);
        let foreign = invoke(&directory.0, "wrong-secret-identity");
        assert_eq!(foreign.status.code(), Some(3));
        assert_eq!(foreign.stderr, failed.stderr);
        directory.write("checkpoint.bin", b"corrupt-secret-checkpoint");
        let corrupt = invoke(&directory.0, "fixture");
        assert_eq!(corrupt.status.code(), Some(3));
        assert_eq!(corrupt.stderr, failed.stderr);
        assert_eq!(
            fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            b"corrupt-secret-checkpoint"
        );
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 3);
    }
}
