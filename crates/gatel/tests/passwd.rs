use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn generate(input: &[u8], args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .arg("passwd")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn password_generation_is_salted_bounded_and_never_echoes_input() {
    let first = generate(b" pass \r\n", &["--stdin", "--cost", "4"]);
    let second = generate(b" pass \n", &["--stdin", "--cost", "4"]);
    for output in [&first, &second] {
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let hash = std::str::from_utf8(&output.stdout).unwrap().trim_end();
        assert_eq!(hash.len(), 60);
        assert!(hash.starts_with("$2b$04$"));
        assert!(bcrypt::verify(" pass ", hash).unwrap());
        assert!(!bcrypt::verify("pass", hash).unwrap());
    }
    assert_ne!(first.stdout, second.stdout);
    for input in [b"".as_slice(), b"private\nmaterial", b"\xff", &[b'x'; 72]] {
        let output = generate(input, &["--stdin", "--cost", "4"]);
        assert_eq!(output.status.code(), Some(3));
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private"));
    }
    for args in [
        vec![],
        vec!["--stdin", "--cost", "3"],
        vec!["--stdin", "--cost", "17"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("passwd")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

struct Gateway {
    child: Child,
    root: PathBuf,
}
impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn generated_hash_is_accepted_by_real_http_basic_auth() {
    let output = generate(b" pass \n", &["--stdin", "--cost", "4"]);
    verify_over_http(output, "bcrypt");
}

#[test]
fn argon2id_capability_cli_and_http_agree() {
    let capabilities = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(capabilities.status.success());
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout)
            .split_whitespace()
            .any(|word| word == "argon2id"),
        cfg!(feature = "argon2")
    );
    #[cfg(feature = "argon2")]
    {
        let output = generate(b" pass \n", &["--stdin", "--algorithm", "argon2id"]);
        verify_over_http(output, "argon2id");
        let output = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .args(["passwd", "--stdin", "--algorithm", "argon2id", "--cost", "4"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    #[cfg(not(feature = "argon2"))]
    {
        let output = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .args(["passwd", "--stdin", "--algorithm", "argon2id"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

fn verify_over_http(output: Output, algorithm: &str) {
    assert!(output.status.success());
    let root = std::env::temp_dir().join(format!("gatel-passwd-http-{algorithm}-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(root.join("user.hash"))
        .unwrap()
        .write_all(&output.stdout)
        .unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        root.join("gatel.kdl"),
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{
 basic-auth {{ user "alice" hash="file:user.hash"; }}
 respond "protected";
}} }}
"#
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .arg("run")
        .arg(root.join("gatel.kdl"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut gateway = Gateway { child, root };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = gateway.child.try_wait().unwrap() {
            let mut diagnostic = String::new();
            gateway
                .child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut diagnostic)
                .unwrap();
            panic!("gateway exited with {status}: {diagnostic}");
        }
        if TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    for (authorization, expected) in [
        ("YWxpY2U6IHBhc3Mg", "200"),
        ("YWxpY2U6cGFzcw==", "401"),
        ("", "401"),
    ] {
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {authorization}\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{response}"
        );
        if expected == "200" {
            assert!(response.ends_with("protected"));
        }
    }
}
