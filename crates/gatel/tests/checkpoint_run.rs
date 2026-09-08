#![cfg(feature = "admin-api")]

use std::path::PathBuf;
use std::process::Command;

struct Process(std::process::Child);

#[path = "support/checkpoint_restart.rs"]
mod restart;
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serving(address: std::net::SocketAddr) -> bool {
    use std::io::{Read, Write};
    let result = (|| -> std::io::Result<String> {
        let timeout = std::time::Duration::from_millis(200);
        let mut stream = std::net::TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut response = String::new();
        stream.take(16 * 1024).read_to_string(&mut response)?;
        Ok(response)
    })();
    result.is_ok_and(|text| text.starts_with("HTTP/1.1 200") && text.ends_with("checkpoint-cli"))
}

#[test]
fn live_checkpoint_process_exits_on_save_failure_and_releases_listener_and_lock() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let directory = Directory::new();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let root = directory.0.join("main.kdl");
    let config = format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint-cli\"; }} }}"
    );
    std::fs::write(&root, &config).unwrap();
    drop(socket);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("run")
            .arg(&root)
            .arg("--checkpoint-journal")
            .arg(&directory.0)
            .args(["--checkpoint-instance", "fixture"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let path = directory.0.join("checkpoint.bin");
    while !path.is_file() || !serving(address) {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "server exited before readiness"
        );
        assert!(Instant::now() < deadline, "server did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let saved = std::fs::read(&path).unwrap();
    // Another real CLI cannot acquire this live process's writer lock.
    assert_eq!(directory.run("fixture").status.code(), Some(3));
    let backup = directory.0.join("confirmed.bin");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(&root, format!("{config}\n// trigger checkpoint save\n")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "watcher failure did not terminate run");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(3));
    let mut error = String::new();
    std::io::Read::read_to_string(&mut process.0.stderr.take().unwrap(), &mut error).unwrap();
    assert!(error.contains("checkpoint watcher stopped"), "{error}");
    assert!(error.contains("persistence"), "{error}");
    assert_eq!(std::fs::read(&backup).unwrap(), saved);
    let released = std::net::TcpListener::bind(address).unwrap();
    drop(released);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    std::fs::write(&root, "site {").unwrap();
    // Reopen only after explicit fixture repair: old confirmed state survives,
    // and graceful error termination released the exclusive writer lock.
    let restored = directory.run("fixture");
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(std::fs::read(&path).unwrap(), saved);
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gatel-checkpoint-cli-{}",
            gatel_model::BootId::generate()
        ));
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

    fn run(&self, instance: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("run")
            .arg(self.0.join("main.kdl"))
            .args(["--dry-run", "--checkpoint-journal"])
            .arg(&self.0)
            .args(["--checkpoint-instance", instance])
            .output()
            .unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn explicit_checkpoint_run_saves_restores_and_rejects_foreign_identity() {
    let directory = Directory::new();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    std::fs::write(
        directory.0.join("main.kdl"),
        format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint-cli\"; }} }}"
        ),
    )
    .unwrap();
    drop(socket);
    let first = directory.run("fixture");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("checkpoint confirmed"));
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    std::fs::write(directory.0.join("main.kdl"), "site {").unwrap();
    let restored = directory.run("fixture");
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    let foreign = directory.run("foreign");
    assert_eq!(foreign.status.code(), Some(3));
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
}

#[test]
fn checkpoint_cli_requires_pair_and_conflicts_with_readonly_bootstrap() {
    for args in [
        vec!["run", "--checkpoint-journal", "unused"],
        vec!["run", "--checkpoint-instance", "fixture"],
        vec![
            "run",
            "--checkpoint-journal",
            "unused",
            "--checkpoint-instance",
            "fixture",
            "--bootstrap-journal",
            "unused",
            "--bootstrap-instance",
            "fixture",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}
