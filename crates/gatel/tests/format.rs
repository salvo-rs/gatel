//! Formatting through the shipped binary, without compiling or following imports.
use std::path::PathBuf;
use std::process::{Command, Output};

#[path = "support/cli_process.rs"]
mod process_output;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gatel fmt {} {}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, check: bool) -> Output {
        self.run_args(if check { &["fmt", "--check"] } else { &["fmt"] })
    }
    fn run_args(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gatel"));
        command.current_dir(&self.0).args(args);
        eprintln!("format fixture command={args:?}");
        process_output::output(&mut command)
    }
    fn write(&self, text: impl AsRef<[u8]>) {
        std::fs::write(self.0.join("gatel.kdl"), text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn preserves_comments_imports_and_plugin_syntax_without_writing_and_is_idempotent() {
    let fixture = Fixture::new();
    let source = "// operator comment\nimport \"missing.kdl\"\nsite \"*\" { route \"/*\" { custom-plugin secret=\"  密码  \"; }; }\n";
    fixture.write(source);
    let check = fixture.run(true);
    assert_eq!(check.status.code(), Some(1), "{check:?}");
    assert!(check.stdout.is_empty());
    let output = fixture.run(false);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let formatted = String::from_utf8(output.stdout).unwrap();
    for retained in ["// operator comment", "missing.kdl", "custom-plugin", "  密码  "] {
        assert!(formatted.contains(retained), "{formatted}");
    }
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("gatel.kdl")).unwrap(),
        source
    );
    assert!(!fixture.0.join("missing.kdl").exists());
    fixture.write(&formatted);
    let check = fixture.run(true);
    assert!(check.status.success(), "{check:?}");
    assert!(check.stdout.is_empty() && check.stderr.is_empty());
    assert_eq!(fixture.run(false).stdout, formatted.as_bytes());
}

#[test]
fn invalid_utf8_syntax_excessive_nesting_and_size_have_no_partial_output() {
    let fixture = Fixture::new();
    for source in [
        vec![0xff],
        b"site {".to_vec(),
        "node {".repeat(1000).into_bytes(),
        format!("node {}1\n", "/- ".repeat(1000)).into_bytes(),
        format!("{}x{}", "/*".repeat(1000), "*/".repeat(1000)).into_bytes(),
        format!("/* {} */", "*".repeat(10_000)).into_bytes(),
        vec![b' '; 4 * 1024 * 1024 + 1],
    ]
    .into_iter()
    .enumerate()
    {
        let (case, source) = source;
        eprintln!("format invalid-input case={case} bytes={}", source.len());
        fixture.write(&source);
        for check in [false, true] {
            let output = fixture.run(check);
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(output.stdout.is_empty());
            assert!(!output.stderr.is_empty());
        }
        assert_eq!(std::fs::read(fixture.0.join("gatel.kdl")).unwrap(), source);
    }
}

#[test]
fn validate_also_rejects_comment_complexity_without_aborting() {
    let fixture = Fixture::new();
    for source in [
        format!("{}x{}", "/*".repeat(1000), "*/".repeat(1000)),
        format!("/* {} */", "*".repeat(10_000)),
        format!("node {}1\n", "/- /* note */ ".repeat(1000)),
    ] {
        fixture.write(source);
        let output = fixture.run_args(&["validate"]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("parser steps"),
            "{output:?}"
        );
    }
}

#[test]
fn missing_file_and_directory_are_rejected() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(false).status.code(), Some(1));
    std::fs::create_dir(fixture.0.join("gatel.kdl")).unwrap();
    let output = fixture.run(false);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn alternate_line_endings_cannot_hide_deep_configuration_from_any_cli_parser() {
    let fixture = Fixture::new();
    for newline in [
        "\n", "\r\n", "\r", "\u{000b}", "\u{000c}", "\u{0085}", "\u{2028}", "\u{2029}",
    ] {
        fixture.write(format!(
            "// comment{newline}{}{}",
            "node {".repeat(1000),
            "}".repeat(1000)
        ));
        for arguments in [&["fmt"][..], &["fmt", "--check"], &["validate"]] {
            eprintln!("format newline-case={newline:?}");
            let output = fixture.run_args(arguments);
            assert_eq!(
                output.status.code(),
                Some(1),
                "{newline:?} {arguments:?}: {output:?}"
            );
            let diagnostics = if arguments[0] == "validate" {
                &output.stdout
            } else {
                &output.stderr
            };
            assert!(
                String::from_utf8_lossy(diagnostics).contains("deeper than"),
                "{output:?}"
            );
        }
    }
}
