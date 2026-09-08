//! Multiple file frontends through the shipped command-line parser.
use std::process::Command;

#[test]
fn validate_and_explain_compile_the_complete_named_source_union() {
    let directory = std::env::temp_dir().join(format!("gatel-cli-sources-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    // Configured ports must be non-zero. Reserve an OS-assigned port until
    // immediately before the child prepares its listeners. Unlike in-process
    // tests this CLI cannot adopt our socket; a small handoff race remains.
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    std::fs::write(
        directory.join("gatel.kdl"),
        format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ proxy upstream=\"shared\"; }} }}"
        ),
    )
    .unwrap();
    std::fs::write(
        directory.join("backends.kdl"),
        "upstream \"shared\" { endpoint \"127.0.0.1:9000\"; }",
    )
    .unwrap();
    for command in ["validate", "explain"] {
        let output = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .current_dir(&directory)
            .args([command, "--source", "backends=backends.kdl"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!output.stdout.is_empty());
        let missing = Command::new(env!("CARGO_BIN_EXE_gatel"))
            .current_dir(&directory)
            .arg(command)
            .output()
            .unwrap();
        assert_eq!(missing.status.code(), Some(1));
    }
    let compare = |before: &str, after: &str| {
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .current_dir(&directory)
            .args([
                "diff",
                "gatel.kdl",
                "gatel.kdl",
                "--before-source",
                before,
                "--after-source",
                after,
            ])
            .output()
            .unwrap()
    };
    let same = compare("backends=backends.kdl", "backends=backends.kdl");
    assert!(same.status.success(), "{same:?}");
    assert_eq!(String::from_utf8_lossy(&same.stdout), "no change\n");
    let moved = compare("backends=backends.kdl", "renamed=backends.kdl");
    assert!(moved.status.success(), "{moved:?}");
    let moved_text = String::from_utf8_lossy(&moved.stdout);
    assert!(
        moved_text.contains("owner file:backends -> file:renamed"),
        "{moved_text}"
    );
    assert!(moved_text.contains("all reused"));
    std::fs::write(
        directory.join("next.kdl"),
        "upstream \"shared\" { endpoint \"127.0.0.1:9001\"; }",
    )
    .unwrap();
    let changed = compare("backends=backends.kdl", "backends=next.kdl");
    assert!(changed.status.success(), "{changed:?}");
    assert!(String::from_utf8_lossy(&changed.stdout).contains("restarted:"));
    let invalid_after = compare("backends=backends.kdl", "backends=missing.kdl");
    assert_eq!(invalid_after.status.code(), Some(1));
    for (file, body) in [
        ("response-old.kdl", "private-old-response"),
        ("response-new.kdl", "private-new-response"),
    ] {
        std::fs::write(
            directory.join(file),
            format!(
                "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}"
            ),
        )
        .unwrap();
    }
    let response_change = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .current_dir(&directory)
        .args(["diff", "response-old.kdl", "response-new.kdl"])
        .output()
        .unwrap();
    assert!(response_change.status.success(), "{response_change:?}");
    let response_text = String::from_utf8_lossy(&response_change.stdout);
    assert!(response_text.contains("configuration changed"), "{response_text}");
    assert!(response_text.contains("all reused"));
    assert!(!response_text.contains("private-old-response"));
    assert!(!response_text.contains("private-new-response"));
    // Compilation above succeeds while this socket is held: diff must not bind.
    drop(reservation);
    let dry_run = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .current_dir(&directory)
        .args(["run", "--source", "backends=backends.kdl", "--dry-run"])
        .output()
        .unwrap();
    assert!(dry_run.status.success(), "{dry_run:?}");
    let duplicate = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .current_dir(&directory)
        .args([
            "validate",
            "--source",
            "a=backends.kdl",
            "--source",
            "a=other.kdl",
        ])
        .output()
        .unwrap();
    assert_eq!(duplicate.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("duplicate source name"));
    let invalid = Command::new(env!("CARGO_BIN_EXE_gatel"))
        .current_dir(&directory)
        .args(["run", "--source", "main=backends.kdl", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
}
