//! The packaged executable probes the running generation, not the KDL on disk.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    child: Child,
    directory: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn command() -> Command {
    let command = Command::new(env!("CARGO_BIN_EXE_gatel"));
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x08000000);
        command
    };
    command
}

fn response(address: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

#[test]
fn cli_probe_uses_live_runtime_even_when_disk_configuration_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let directory = std::env::temp_dir().join(format!("gatel-cli-probe-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let config = directory.join("gatel.kdl");
    std::fs::write(&config, format!("global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/readyz\" {{ health \"ready\"; }}; route \"/livez\" {{ health \"live\"; }}; }}")).unwrap();
    let log = std::fs::File::create(directory.join("server.log")).unwrap();
    drop(listener);
    let child = command()
        .args(["run", config.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut fixture = Fixture { child, directory };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !response(address).is_ok_and(|text| text.starts_with("HTTP/1.1 200")) {
        assert!(fixture.child.try_wait().unwrap().is_none(), "gateway exited");
        assert!(Instant::now() < deadline, "gateway did not serve readiness");
        std::thread::sleep(Duration::from_millis(20));
    }
    let invoke = |path: &str, kind: &str| {
        command()
            .args([
                "probe",
                "--address",
                &address.to_string(),
                "--path",
                path,
                "--kind",
                kind,
            ])
            .output()
            .unwrap()
    };
    assert!(invoke("/readyz", "ready").status.success());
    assert!(invoke("/livez", "live").status.success());
    assert_eq!(invoke("/livez", "ready").status.code(), Some(3));
    std::fs::write(&config, "invalid-node;").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !response(address).is_ok_and(|text| text.contains("degraded")) {
        assert!(
            Instant::now() < deadline,
            "watcher did not report rejected configuration"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        command()
            .args(["validate", config.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    let ready = invoke("/readyz", "ready");
    assert!(ready.status.success(), "{ready:?}");
    assert_eq!(ready.stdout, b"ready\n");
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    assert_eq!(invoke("/readyz", "ready").status.code(), Some(3));
}

#[test]
fn cli_probe_rejects_unsafe_options_without_contacting_a_server() {
    for args in [
        vec!["probe", "--address", "0.0.0.0:8080"],
        vec!["probe", "--address", "192.0.2.1:8080"],
        vec!["probe", "--path", "/readyz\r\nX: injected"],
        vec!["probe", "--timeout-ms", "0"],
    ] {
        assert_eq!(command().args(args).output().unwrap().status.code(), Some(2));
    }
}
