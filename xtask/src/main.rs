//! Repository checks that `cargo` cannot express.
//!
//! Two rules, both from the architecture rather than from taste:
//!
//! - A crate may only depend on the layers below it. The layering is what makes "the compiler cannot touch a
//!   socket" a fact rather than a convention.
//! - Files stay small enough to hold in one's head. This is a target with a hard ceiling, not a style
//!   preference: the previous generation of this project had a 3,142-line configuration parser, and that is
//!   exactly how a codebase stops being reviewable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// What each crate is allowed to depend on directly.
///
/// The point of the low layers is what they *cannot* reach: `gatel-model` has
/// no async runtime and no I/O, and `gatel-plan` cannot open a socket or
/// terminate TLS, so neither can accidentally grow a side entrance into the
/// data plane.
const ALLOWED: &[(&str, &[&str])] = &[
    ("gatel-model", &["blake3"]),
    ("gatel-macros", &["proc-macro2", "quote", "syn", "gatel-model"]),
    ("gatel-kdl", &["gatel-model", "kdl"]),
    // Below everything that can open a socket, so the data plane can write to it.
    ("gatel-observe", &["gatel-model"]),
    (
        "gatel-plan",
        // salvo_core is the project's execution vocabulary; hyper, rustls and
        // tokio::net are transport and belong below.
        &[
            "gatel-model",
            "gatel-macros",
            "gatel-kdl",
            "gatel-observe",
            "salvo_core",
        ],
    ),
    (
        "gatel-state",
        &[
            "gatel-model",
            "gatel-plan",
            "gatel-observe",
            "gatel-kdl",
            "bytes",
            "http",
            "http-body-util",
            "hyper",
            "hyper-util",
            "salvo_core",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-bundle-std",
        &[
            "gatel-model",
            "gatel-macros",
            "gatel-plan",
            "gatel-observe",
            "async-compression",
            "bcrypt",
            "bytes",
            "futures-util",
            "http-body-util",
            "hyper",
            "tokio-util",
            "http",
            "http-body",
            "pin-project-lite",
            "salvo_core",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-http",
        &[
            "gatel-model",
            "gatel-plan",
            "gatel-observe",
            "gatel-state",
            "gatel-tls",
            "gatel-kdl",
            "gatel-bundle-std",
            "bytes",
            "futures-util",
            "http",
            "http-body",
            "http-body-util",
            "hyper",
            "hyper-util",
            "pin-project-lite",
            "salvo_core",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-tls",
        &[
            "gatel-model",
            "gatel-plan",
            "rcgen",
            "rustls",
            "rustls-pemfile",
            "time",
            "tokio",
            "tokio-rustls",
            "tracing",
            "webpki-roots",
        ],
    ),
];

const TARGET_LINES: usize = 600;
const MAX_PRODUCTION_LINES: usize = 800;
const MAX_TEST_LINES: usize = 1200;

fn main() -> ExitCode {
    let root = match repository_root() {
        Some(root) => root,
        None => {
            eprintln!("xtask: could not find the repository root");
            return ExitCode::FAILURE;
        }
    };

    let task = std::env::args().nth(1).unwrap_or_else(|| "check".to_string());
    let failures = match task.as_str() {
        "deps" => check_dependencies(&root),
        "loc" => check_file_sizes(&root),
        "check" => {
            let mut failures = check_dependencies(&root);
            failures.extend(check_file_sizes(&root));
            failures
        }
        other => {
            eprintln!("xtask: unknown task `{other}`; try deps, loc or check");
            return ExitCode::from(2);
        }
    };

    if failures.is_empty() {
        println!("xtask: ok");
        return ExitCode::SUCCESS;
    }
    for failure in &failures {
        eprintln!("xtask: {failure}");
    }
    eprintln!("\nxtask: {} problem(s)", failures.len());
    ExitCode::FAILURE
}

fn repository_root() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        if current.join("Cargo.toml").exists() && current.join("crates").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Read each crate's `[dependencies]` from its manifest and compare against the
/// whitelist. Reading the manifest rather than `cargo metadata` keeps this
/// about *declared* dependencies, which is what the rule is about.
fn check_dependencies(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let allowed: BTreeMap<&str, &[&str]> = ALLOWED.iter().copied().collect();

    for (name, permitted) in &allowed {
        let manifest = root.join("crates").join(name).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            failures.push(format!("{name}: no manifest at {}", manifest.display()));
            continue;
        };
        for dependency in declared_dependencies(&text) {
            if !permitted.contains(&dependency.as_str()) {
                failures.push(format!(
                    "{name} depends on `{dependency}`, which its layer does not allow\n           \
                     if this is deliberate, add it to ALLOWED in xtask and say why"
                ));
            }
        }
    }
    failures
}

/// Parse the `[dependencies]` table only. `[dev-dependencies]` is exempt:
/// a test may reach anywhere, because it does not ship.
fn declared_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[dependencies]";
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            // `serde.workspace = true` and `serde = { .. }` name the same
            // crate; the dotted form is a key path, not a crate name.
            let name = key.trim().trim_matches('"');
            let name = name.split_once('.').map(|(head, _)| head).unwrap_or(name);
            names.push(name.to_string());
        }
    }
    names
}

fn check_file_sizes(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let mut warnings = Vec::new();

    for path in rust_files(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines = production_lines(&text);
        let relative = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        let is_test = relative.contains("tests");
        let limit = if is_test {
            MAX_TEST_LINES
        } else {
            MAX_PRODUCTION_LINES
        };

        if lines > limit {
            failures.push(format!("{relative} is {lines} lines, over the limit of {limit}"));
        } else if !is_test && lines > TARGET_LINES {
            warnings.push(format!(
                "{relative} is {lines} lines, over the target of {TARGET_LINES}"
            ));
        }
    }

    for warning in warnings {
        println!("xtask: note: {warning}");
    }
    failures
}

/// Count production lines only: an inline `#[cfg(test)]` module sits next to
/// the code it tests on purpose, and penalising that would push tests away from
/// what they cover. The rule is about how much production code one file holds.
fn production_lines(text: &str) -> usize {
    match text
        .lines()
        .position(|line| line.trim_start().starts_with("#[cfg(test)]"))
    {
        Some(index) => index,
        None => text.lines().count(),
    }
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("xtask")];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
