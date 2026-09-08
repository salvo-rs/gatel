use std::io::{Read, Write};
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{Command, Directory, Process};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const READ_TOKEN: &str = "fedcba9876543210fedcba9876543210";

fn request(address: SocketAddr, input: &str) -> std::io::Result<String> {
    let timeout = Duration::from_secs(1);
    let mut socket = std::net::TcpStream::connect_timeout(&address, timeout)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    socket.write_all(input.as_bytes())?;
    let mut output = String::new();
    socket.take(32 * 1024).read_to_string(&mut output)?;
    Ok(output)
}

fn version(process: &mut Process, address: SocketAddr) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = request(
            address,
            &format!(
                "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
            ),
        );
        if let Ok(response) = response
            && response.starts_with("HTTP/1.1 200")
        {
            return response
                .lines()
                .find_map(|line| line.strip_prefix("etag: "))
                .unwrap()
                .to_owned();
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "checkpoint CLI exited before Admin readiness"
        );
        assert!(Instant::now() < deadline, "Admin did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn acknowledged_admin_update_survives_kill_and_cold_restart() {
    restart_case(false, false);
}

#[test]
fn rejected_admin_intent_keeps_serving_checkpoint_and_can_be_corrected_after_restart() {
    restart_case(true, false);
}

#[test]
fn compacted_checkpoint_preserves_rejected_intent_and_serves_after_cold_restart() {
    restart_case(true, true);
}

fn restart_case(rejected: bool, compact: bool) {
    let directory = Directory::new();
    let business = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = business.local_addr().unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_address = admin.local_addr().unwrap();
    let root = directory.0.join("main.kdl");
    let token = directory.0.join("admin.token");
    let read_token = directory.0.join("admin-read.token");
    std::fs::write(&token, TOKEN).unwrap();
    std::fs::write(&read_token, READ_TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&read_token, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(
        &root,
        format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint-cli\"; }} }}"
        ),
    )
    .unwrap();
    drop(business);
    drop(admin);
    let start = |checkpoint: &std::path::Path| {
        Process(
            Command::new(env!("CARGO_BIN_EXE_gatel"))
                .arg("run")
                .arg(&root)
                .arg("--checkpoint-journal")
                .arg(checkpoint)
                .args([
                    "--checkpoint-instance",
                    "fixture",
                    "--admin-listen",
                    &admin_address.to_string(),
                ])
                .arg("--admin-token-file")
                .arg(&token)
                .arg("--admin-read-token-file")
                .arg(&read_token)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let mut first = start(&directory.0);
    let initial = version(&mut first, admin_address);
    for path in ["/v1/config/version", "/v1/config/status"] {
        let response = request(
            admin_address,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {READ_TOKEN}\r\n\r\n"),
        )
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains(READ_TOKEN));
    }
    let denied = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {READ_TOKEN}\r\nIf-Match: {initial}\r\nContent-Type: application/kdl\r\nContent-Length: 100\r\n\r\n")).unwrap();
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    assert_eq!(version(&mut first, admin_address), initial);
    let body = "site \"admin-proof.invalid\" { route \"/\" { respond \"acknowledged-admin\"; } }";
    let response = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {initial}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}", body.len())).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let confirmed = version(&mut first, admin_address);
    if rejected {
        let invalid = "unsupported-root;";
        let response = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {confirmed}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{invalid}", invalid.len())).unwrap();
        assert!(response.starts_with("HTTP/1.1 422"), "{response}");
        assert_ne!(version(&mut first, admin_address), confirmed);
    }
    let saved = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert_eq!(std::fs::read(directory.0.join("checkpoint.bin")).unwrap(), saved);
    if rejected {
        let inspection = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .args(["journal-inspect", "--directory"])
            .arg(&directory.0)
            .args(["--instance", "fixture"])
            .output()
            .unwrap();
        assert!(
            inspection.status.success(),
            "{}",
            String::from_utf8_lossy(&inspection.stderr)
        );
        let report = String::from_utf8(inspection.stdout).unwrap();
        assert!(report.contains("checkpoint_observed_differs=true"), "{report}");
    }
    let destination = directory.0.join("compacted");
    if compact {
        let source_log = std::fs::read(directory.0.join("intent.log")).unwrap();
        let compact_command = || {
            Command::new(env!("CARGO_BIN_EXE_gatel"))
                .args(["journal-compact", "--directory"])
                .arg(&directory.0)
                .args(["--instance", "fixture", "--destination"])
                .arg(&destination)
                .output()
                .unwrap()
        };
        let result = compact_command();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!String::from_utf8_lossy(&result.stdout).contains(TOKEN));
        assert_eq!(
            compact_command().status.code(),
            Some(3),
            "must not overwrite destination"
        );
        assert_eq!(std::fs::read(directory.0.join("checkpoint.bin")).unwrap(), saved);
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), source_log);
    }
    // Startup must not reconstruct this Admin route from a current file.
    std::fs::write(&root, "site {").unwrap();
    let mut second = start(if compact { &destination } else { &directory.0 });
    let current = version(&mut second, admin_address);
    assert_ne!(confirmed, current, "ETags must include a fresh boot identity");
    let status = request(
        admin_address,
        &format!(
            "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
        ),
    )
    .unwrap();
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(
        status
            .lines()
            .any(|line| line == format!("x-gatel-observed-differs: {rejected}")),
        "{status}"
    );
    let response = request(
        address,
        "GET / HTTP/1.1\r\nHost: admin-proof.invalid\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("acknowledged-admin"),
        "{response}"
    );
    let stale = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {confirmed}\r\nContent-Type: application/kdl\r\nContent-Length: 0\r\n\r\n")).unwrap();
    assert!(stale.starts_with("HTTP/1.1 412"), "{stale}");
    if rejected {
        let repaired = request(admin_address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {current}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}", body.len())).unwrap();
        assert!(repaired.starts_with("HTTP/1.1 200"), "{repaired}");
        assert_ne!(version(&mut second, admin_address), current);
    }
    second.0.kill().unwrap();
    second.0.wait().unwrap();
}
