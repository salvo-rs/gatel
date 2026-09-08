//! The actual host binary must expose exactly the plugins it executes.
use std::process::Command;

#[test]
fn decode_capability_and_validation_require_the_optional_feature() {
    let capabilities = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(capabilities.status.success());
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout)
            .split_whitespace()
            .any(|word| word == "decode"),
        cfg!(feature = "decode")
    );
    let validation = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args([
            "validate",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/decode/examples/gatel.kdl"
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        validation.status.success(),
        cfg!(feature = "decode"),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
}

#[test]
fn optional_plugin_capabilities_and_validation_agree() {
    let capabilities = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(capabilities.status.success());
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout)
            .split_whitespace()
            .any(|word| word == "cgi"),
        cfg!(feature = "cgi")
    );
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout)
            .split_whitespace()
            .any(|word| word == "scgi"),
        cfg!(feature = "scgi")
    );
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout).contains("fastcgi"),
        cfg!(feature = "fastcgi")
    );
    assert_eq!(
        String::from_utf8_lossy(&capabilities.stdout).contains("maintenance"),
        cfg!(feature = "maintenance")
    );
    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/maintenance/examples/maintenance.kdl"
    );
    let validation = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args(["validate", example])
        .output()
        .unwrap();
    assert_eq!(
        validation.status.success(),
        cfg!(feature = "maintenance"),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
}

#[test]
fn fastcgi_configuration_is_available_only_when_compiled_in() {
    let validation = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args([
            "validate",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/fastcgi/examples/gatel.kdl"
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        validation.status.success(),
        cfg!(feature = "fastcgi"),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
}

#[test]
fn process_cgi_validation_requires_feature_and_supported_platform() {
    let validation = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args([
            "validate",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/cgi/examples/gatel.kdl"
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        validation.status.success(),
        cfg!(all(feature = "cgi", windows)),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
}

#[test]
fn scgi_configuration_is_available_only_when_compiled_in() {
    let validation = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args([
            "validate",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/scgi/examples/gatel.kdl"
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        validation.status.success(),
        cfg!(feature = "scgi"),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
}

#[cfg(feature = "maintenance")]
mod live {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;

    struct Gateway {
        child: Child,
        directory: PathBuf,
    }
    impl Drop for Gateway {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn request(port: u16, method: &str) -> std::io::Result<String> {
        let mut socket =
            TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        write!(
            socket,
            "{method} / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )?;
        let mut response = String::new();
        socket.read_to_string(&mut response)?;
        Ok(response)
    }

    fn await_status(port: u16, status: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Ok(response) = request(port, "GET")
                && response.starts_with(status)
            {
                return response;
            }
            assert!(Instant::now() < deadline, "gateway did not reach {status}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn plugin_serves_head_and_reloads_through_the_real_host() {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let directory = std::env::temp_dir().join(format!(
            "gatel-plugin-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let config = directory.join("gatel.kdl");
        let source = |enabled| {
            format!(
                "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{\n maintenance enabled=#{enabled} retry-after=42\n route \"/*\" {{ respond \"online\"; }}\n}}\n"
            )
        };
        std::fs::write(&config, source(true)).unwrap();
        drop(reservation);
        let _gateway = Gateway {
            child: Command::new(env!("CARGO_BIN_EXE_gatel"))
                .arg("run")
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
            directory,
        };
        let response = await_status(port, "HTTP/1.1 503");
        assert!(response.to_ascii_lowercase().contains("retry-after: 42\r\n"));
        assert!(response.ends_with("Service temporarily unavailable\n"));
        let head = request(port, "HEAD").unwrap();
        assert!(head.starts_with("HTTP/1.1 503"));
        assert_eq!(head.split_once("\r\n\r\n").unwrap().1, "");
        std::fs::write(&config, source(false)).unwrap();
        assert!(await_status(port, "HTTP/1.1 200").ends_with("online"));
        std::fs::write(&config, source(true)).unwrap();
        await_status(port, "HTTP/1.1 503");
    }
}
