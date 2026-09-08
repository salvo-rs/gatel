//! Explicit external conformance gate. Ordinary cargo test never starts Docker.
#![cfg(feature = "argon2")]
use std::io::Write;
use std::process::{Command, Stdio};

use gatel_password::{Algorithm, Credential, generate};

const IMAGE: &str = "php@sha256:59fa733c9af643a122f8a9976119460e35ce76dd0a3f2b9c8f75af8e361a54e2";
const SCRIPT: &str = r#"
$hash = trim(stream_get_contents(STDIN));
$password = match ($argv[2]) {
    "spaces" => " fixture ",
    "unicode" => " 密码🔐café ",
    "boundary" => str_repeat("x", 71),
};
if (!password_verify($password, $hash)) { exit(10); }
if (password_verify($password . "!", $hash)) { exit(11); }
if (trim($password) !== $password && password_verify(trim($password), $hash)) { exit(12); }
if ($argv[1] === "bcrypt") {
    echo password_hash($password, PASSWORD_BCRYPT, ["cost"=>4]);
} else {
    echo password_hash($password, PASSWORD_ARGON2ID, ["memory_cost"=>19456,"time_cost"=>2,"threads"=>1]);
}
echo PHP_EOL;
fwrite(STDERR, "PHP ".PHP_VERSION.PHP_EOL);
"#;

struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

#[test]
#[ignore = "requires Docker and the pinned PHP image already present; explicitly run --ignored"]
fn php_and_gatel_verify_each_others_bcrypt_and_argon2id_hashes() {
    for (fixture, password) in [
        ("spaces", " fixture ".to_owned()),
        ("unicode", " 密码🔐café ".to_owned()),
        ("boundary", "x".repeat(71)),
    ] {
        verify_fixture(fixture, &password);
    }
}

fn verify_fixture(fixture: &str, password: &str) {
    for (name, algorithm, cost) in [
        ("bcrypt", Algorithm::Bcrypt, Some(4)),
        ("argon2id", Algorithm::Argon2id, None),
    ] {
        let hash = generate(algorithm, password.as_bytes(), cost).unwrap();
        let created = Command::new("docker")
            .args([
                "create",
                "--pull=never",
                "-i",
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--memory=128m",
                "--pids-limit=16",
                IMAGE,
                "php",
                "-r",
                SCRIPT,
                "--",
                name,
                fixture,
            ])
            .output()
            .expect("Docker CLI is required");
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
        assert!(id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        // Only the ID returned by our own successful create can be removed.
        let container = Container(id);
        let mut child = Command::new("docker")
            .args(["start", "-ai", &container.0])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let written = child.stdin.take().unwrap().write_all(hash.as_bytes());
        let output = child.wait_with_output().unwrap();
        written.unwrap();
        assert!(
            output.status.success(),
            "{name}/{fixture}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!(
            "{name}/{fixture}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let hash = String::from_utf8(output.stdout).unwrap();
        let credential = Credential::parse(hash.trim_end()).unwrap();
        assert!(credential.verify(password.as_bytes()));
        if password.trim() != password {
            assert!(!credential.verify(password.trim().as_bytes()));
        }
        assert!(!credential.verify(format!("{password}!").as_bytes()));
    }
}
