//! Real CLI watcher lifecycle; Unix additionally exercises graceful SIGTERM.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn response(address: SocketAddr) -> std::io::Result<String> {
    request(
        address,
        "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    )
}

fn request(address: SocketAddr, input: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(input.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

#[cfg(feature = "admin-api")]
#[test]
fn explicit_admin_cli_writes_and_file_reload_share_a_lifecycle() {
    let directory = Directory(std::env::temp_dir().join(format!("gatel-admin-cli-{}", std::process::id())));
    std::fs::create_dir(&directory.0).unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = http.local_addr().unwrap();
    let admin = TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_address = admin.local_addr().unwrap();
    let config = |body| {
        format!("global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}")
    };
    std::fs::write(directory.0.join("main.kdl"), config("before")).unwrap();
    let token = "0123456789abcdef0123456789abcdef";
    let token_path = directory.0.join("admin.token");
    std::fs::write(&token_path, format!("{token}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let log = directory.0.join("events.log");
    let output = std::fs::File::create(&log).unwrap();
    drop(http);
    drop(admin);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .current_dir(&directory.0)
            .args([
                "run",
                "main.kdl",
                "--admin-listen",
                &admin_address.to_string(),
                "--admin-token-file",
                "admin.token",
            ])
            .env("GATEL_LOG", "gatel=info")
            .env("GATEL_LOG_FORMAT", "json")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let query = format!(
        "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    wait_until(&mut process, &log, || {
        request(admin_address, &query).is_ok_and(|r| r.starts_with("HTTP/1.1 200"))
    });
    let version = request(admin_address, &query).unwrap();
    let tag = version
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    let body = "upstream \"runtime/cli\" { endpoint \"127.0.0.1:2\"; }";
    let update = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}", body.len())).unwrap();
    assert!(update.starts_with("HTTP/1.1 200"), "{update}");
    std::fs::write(directory.0.join("main.kdl"), config("after")).unwrap();
    wait_until(&mut process, &log, || {
        response(address).is_ok_and(|r| r.ends_with("after"))
    });
    let final_version = request(admin_address, &query).unwrap();
    let final_tag = final_version
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    assert_ne!(tag, final_tag);
    #[cfg(unix)]
    {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(process.0.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "admin CLI did not drain");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    {
        process.0.kill().unwrap();
        process.0.wait().unwrap();
    }
    assert!(!std::fs::read_to_string(log).unwrap().contains(token));
    assert!(TcpStream::connect_timeout(&admin_address, Duration::from_millis(200)).is_err());
}

fn wait_until(process: &mut Process, log: &std::path::Path, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = process.0.try_wait().unwrap();
        assert!(
            status.is_none(),
            "server exited: {status:?}; {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        if ready() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition timed out; {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn named_source_reload_rejection_recovery_and_process_shutdown() {
    let directory = Directory(std::env::temp_dir().join(format!("gatel-run-reload-{}", std::process::id())));
    std::fs::create_dir(&directory.0).unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    let root = directory.0.join("gatel.kdl");
    let routes = directory.0.join("routes.kdl");
    let log = directory.0.join("events.log");
    std::fs::write(&root, format!("global {{ http \"{address}\"; }}")).unwrap();
    let config = |body: &str| format!("site \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}");
    std::fs::write(&routes, config("first")).unwrap();
    let output = std::fs::File::create(&log).unwrap();
    // CLI cannot adopt the reserved socket; this handoff retains a small race.
    drop(reserved);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .current_dir(&directory.0)
            .args(["run", "gatel.kdl", "--source", "routes=routes.kdl"])
            .env("GATEL_LOG", "gatel=info")
            .env("GATEL_LOG_FORMAT", "json")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    wait_until(&mut process, &log, || {
        response(address).is_ok_and(|r| r.ends_with("first"))
    });
    std::fs::write(&routes, "site {\n").unwrap();
    wait_until(&mut process, &log, || {
        std::fs::read_to_string(&log).is_ok_and(|r| r.contains("config_rejected"))
    });
    assert!(response(address).unwrap().ends_with("first"));
    // The diagnostic log lives beside configuration files. Its own writes must
    // not turn a rejected candidate into an event-driven reload feedback loop.
    let rejected = || {
        std::fs::read_to_string(&log)
            .unwrap()
            .matches("config_rejected")
            .count()
    };
    let before = rejected();
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        rejected() <= before + 1,
        "diagnostic writes repeatedly retriggered rejection"
    );
    std::fs::write(&routes, config("recovered")).unwrap();
    wait_until(&mut process, &log, || {
        response(address).is_ok_and(|r| r.ends_with("recovered"))
    });
    #[cfg(unix)]
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(process.0.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    // No synthetic claim of Windows console Ctrl+C coverage.
    #[cfg(not(unix))]
    process.0.kill().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            #[cfg(unix)]
            assert!(status.success(), "{status:?}");
            #[cfg(not(unix))]
            let _ = status;
            break;
        }
        assert!(Instant::now() < deadline, "process did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    #[cfg(unix)]
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("shutdown_complete")
    );
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
}
