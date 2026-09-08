//! Static helper through the real binary, sharing the production file-server.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("gatel serve {} {sequence}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
}
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
fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gatel"))
}
fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn request(port: u16, method: &str, path: &str, headers: &str) -> String {
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn files_head_ranges_browsing_and_changes_use_the_standard_handler() {
    let directory = Directory::new();
    std::fs::write(directory.0.join("asset.txt"), "abcdef").unwrap();
    std::fs::write(directory.0.join(".secret"), "hidden-value").unwrap();
    // A malformed local config must not be parsed, watched, or overwritten.
    std::fs::write(directory.0.join("gatel.kdl"), "not a valid config {{{").unwrap();
    for browse in [false, true] {
        let port = port();
        let mut command = command();
        command
            .arg("serve")
            .arg(&directory.0)
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if browse {
            command.arg("--browse");
        }
        let mut process = Process(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "server exited before readiness"
            );
            if TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "server never became ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request(port, "GET", "/asset.txt", "").ends_with("abcdef"));
        assert!(request(port, "HEAD", "/asset.txt", "").ends_with("\r\n\r\n"));
        let range = request(port, "GET", "/asset.txt", "Range: bytes=1-3\r\n");
        assert!(range.starts_with("HTTP/1.1 206"), "{range}");
        assert!(range.ends_with("bcd"));
        let listing = request(port, "GET", "/", "");
        if browse {
            assert!(listing.contains("asset.txt"), "{listing}");
        } else {
            assert!(listing.starts_with("HTTP/1.1 404"), "{listing}");
        }
        let hidden = request(port, "GET", "/.secret", "");
        assert!(!hidden.contains("hidden-value"));
        std::fs::write(directory.0.join("index.html"), "new index").unwrap();
        assert!(request(port, "GET", "/", "").ends_with("new index"));
        std::fs::remove_file(directory.0.join("index.html")).unwrap();
        #[cfg(unix)]
        {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(process.0.id()).unwrap()),
                nix::sys::signal::Signal::SIGTERM,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = process.0.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                assert!(Instant::now() < deadline, "SIGTERM did not stop serve");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.0.join("gatel.kdl")).unwrap(),
        "not a valid config {{{"
    );
}

#[test]
fn dry_run_validates_root_binds_and_reports_usage_errors() {
    let directory = Directory::new();
    let output = command()
        .arg("serve")
        .arg(&directory.0)
        .args(["--port", &port().to_string(), "--dry-run"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(std::fs::read_dir(&directory.0).unwrap().next().is_none());
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = command()
        .arg("serve")
        .arg(&directory.0)
        .args([
            "--port",
            &busy.local_addr().unwrap().port().to_string(),
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    std::fs::write(directory.0.join("file"), "not a directory").unwrap();
    for root in [directory.0.join("file"), directory.0.join("missing")] {
        let output = command()
            .arg("serve")
            .arg(root)
            .arg("--dry-run")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3), "{output:?}");
    }
    for args in [["--port", "0"], ["--port", "65536"], ["--listen", "localhost"]] {
        let output = command()
            .arg("serve")
            .args(args)
            .arg("--dry-run")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
    }
}

#[cfg(unix)]
#[test]
fn static_root_is_a_filesystem_path_not_interpolated_configuration() {
    use std::os::unix::ffi::OsStringExt;
    let directory = Directory::new();
    for name in [
        std::ffi::OsString::from("quotes\" and\nnewlines { }"),
        std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()),
    ] {
        let root = directory.0.join(name);
        std::fs::create_dir(&root).unwrap();
        let output = command()
            .arg("serve")
            .arg(root)
            .args(["--port", &port().to_string(), "--dry-run"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}
