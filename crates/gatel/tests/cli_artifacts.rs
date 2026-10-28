//! Exercise generated artifacts through the shipped executable, not a duplicate CLI.
use std::process::{Command, Output};

fn invoke(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatel"))
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("GATEL_LOG", "trace")
        .output()
        .unwrap()
}

fn artifact(arguments: &[&str]) -> String {
    let output = invoke(arguments);
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn every_supported_shell_has_current_commands_and_run_flags() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let text = artifact(&["completions", shell]);
        for name in [
            "gatel",
            "validate",
            "probe",
            "fmt",
            "explain",
            "capabilities",
            "precompress",
            "man-page",
            "dry-run",
        ] {
            assert!(text.contains(name), "{shell}: missing {name}");
        }
        assert_eq!(
            text,
            artifact(&["completions", shell]),
            "nondeterministic {shell}"
        );
    }
}

#[test]
fn root_and_subcommand_manuals_follow_actual_parser() {
    let root = artifact(&["man-page"]);
    assert!(root.contains(".TH gatel 1"), "{root}");
    assert!(root.contains("SUBCOMMANDS"));
    for command in [
        "run",
        "serve",
        "passwd",
        "precompress",
        "validate",
        "probe",
        "fmt",
        "explain",
        "diff",
        "capabilities",
        "completions",
        "man-page",
        "trust",
        "untrust",
    ] {
        assert!(
            root.contains(&command.replace('-', "\\-")),
            "root manual missing {command}"
        );
        let page = artifact(&["man-page", command]);
        assert!(page.contains("SYNOPSIS"), "{page}");
        assert_eq!(page, artifact(&["man-page", command]));
    }
    let run = artifact(&["man-page", "run"]);
    assert!(run.contains("dry\\-run"), "{run}");
    assert!(run.contains("gatel.kdl"), "{run}");
    assert!(artifact(&["man-page", "trust"]).contains("directory"));
    let serve = artifact(&["man-page", "serve"]);
    for value in ["127.0.0.1", "8080", "browse", "listen", "dry\\-run"] {
        assert!(serve.contains(value), "serve manual missing {value}");
    }
}

#[test]
fn invalid_artifact_requests_are_usage_errors_without_partial_output() {
    for args in [
        vec!["completions"],
        vec!["completions", "unknown-shell"],
        vec!["man-page", "unknown-command"],
        vec!["man-page", "run", "unknown-child"],
    ] {
        let output = invoke(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
