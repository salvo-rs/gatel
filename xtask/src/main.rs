//! Repository checks that `cargo` cannot express.
//!
//! Four rules, all from the architecture rather than from taste:
//!
//! - A crate may only depend on the layers below it. The layering is what makes "the compiler cannot touch a
//!   socket" a fact rather than a convention.
//! - Files stay small enough to hold in one's head. This is a target with a hard ceiling, not a style
//!   preference: the previous generation of this project had a 3,142-line configuration parser, and that is
//!   exactly how a codebase stops being reviewable.
//! - Every event the code emits is in `docs/EVENTS.md`, and every event documented there is emitted.
//! - Every configuration field the compiler produces is read by something above it. A knob that is accepted
//!   and then ignored is worse than one that was never offered, because the operator who sets it stops
//!   looking for the real problem — and this rule exists because three of them were found that way.

use std::collections::{BTreeMap, BTreeSet};
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
    ("gatel-observe", &["gatel-model", "time"]),
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
            // The pool dials backends, and some of them speak TLS. The client
            // side of TLS lives with the server side, not in a second place.
            "gatel-tls",
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
            // The ACME protocol itself. Same organisation, same licence, and
            // already what v1 uses: one implementation rather than two.
            "certon",
            "async-trait",
            "gatel-observe",
            "rcgen",
            "rustls",
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
        "events" => check_events(&root),
        "inert" => check_inert_configuration(&root),
        "check" => {
            let mut failures = check_dependencies(&root);
            failures.extend(check_file_sizes(&root));
            failures.extend(check_events(&root));
            failures.extend(check_inert_configuration(&root));
            failures
        }
        other => {
            eprintln!("xtask: unknown task `{other}`; try deps, loc, events, inert or check");
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

/// Fields that are genuinely read, in a way this check cannot see.
///
/// Empty on purpose. Anything added here needs a sentence saying where the
/// field is actually consulted, because the whole point of the rule is that
/// "it is used somewhere, trust me" is what let three dead knobs survive.
const READ_ELSEWHERE: &[&str] = &[];

/// The files the compiler's output is described in. A `pub` field on a type
/// here is something an operator can write in a configuration file.
const CONFIGURATION: &[&str] = &[
    "spec.rs",
    "plan.rs",
    "upstream/mod.rs",
    "matcher.rs",
    "directive.rs",
];

/// Everything above the compiler. A field has to reach one of these to do
/// anything at all.
const CONSUMERS: &[&str] = &[
    "gatel-state",
    "gatel-http",
    "gatel-tls",
    "gatel-observe",
    "gatel-bundle-std",
    "gatel",
];

/// Find configuration the compiler carries and nothing acts on.
///
/// Deliberately crude: it collects `pub` field names from the plan's public
/// types and then checks whether the identifier appears anywhere in the crates
/// above. That cannot prove a field is *applied* — only that something at least
/// mentions it — but it is enough to catch a whole block of settings that is
/// decoded, validated, hashed and then dropped, which is the failure this is
/// for.
fn check_inert_configuration(root: &Path) -> Vec<String> {
    let mut declared: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in CONFIGURATION {
        let path = root.join("crates/gatel-plan/src").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut owner = String::new();
        for line in production(&text).lines() {
            let line = line.trim();
            if let Some(rest) = line
                .strip_prefix("pub struct ")
                .or_else(|| line.strip_prefix("pub enum "))
            {
                owner = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                continue;
            }
            let Some(rest) = line.strip_prefix("pub ") else {
                continue;
            };
            let Some((field, _)) = rest.split_once(": ") else {
                continue;
            };
            if owner.is_empty()
                || field.is_empty()
                || !field
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            {
                continue;
            }
            declared
                .entry(field.to_string())
                .or_default()
                .insert(owner.clone());
        }
    }

    let mut mentioned = BTreeSet::new();
    for crate_name in CONSUMERS {
        for path in rust_files_under(&root.join("crates").join(crate_name).join("src")) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
                mentioned.insert(word.to_string());
            }
        }
    }

    declared
        .into_iter()
        .filter(|(field, _)| !mentioned.contains(field) && !READ_ELSEWHERE.contains(&field.as_str()))
        .map(|(field, owners)| {
            let owners: Vec<&str> = owners.iter().map(String::as_str).collect();
            format!(
                "`{field}` on {} is configuration nothing above the compiler reads\n           \
                 either apply it, refuse it at compile time, or delete it",
                owners.join(" and ")
            )
        })
        .collect()
}

/// Everything before an inline `#[cfg(test)]`, so a field named only by a test
/// fixture is not mistaken for a declaration.
fn production(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(index) => &text[..index],
        None => text,
    }
}

fn rust_files_under(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found
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

/// Every structured event the code emits must be written down, and everything
/// written down must still be emitted.
///
/// An event name is an interface: something is alerting on it. Renaming one
/// without noticing is the kind of change that is only discovered when the
/// alert does not fire.
fn check_events(root: &Path) -> Vec<String> {
    let documented = match std::fs::read_to_string(root.join("docs/EVENTS.md")) {
        Ok(text) => documented_events(&text),
        Err(error) => return vec![format!("cannot read docs/EVENTS.md: {error}")],
    };
    let emitted = emitted_events(root);

    let mut failures = Vec::new();
    for (target, event) in &emitted {
        if !documented.contains(&(target.clone(), event.clone())) {
            failures.push(format!(
                "event `{event}` on `{target}` is emitted but not in docs/EVENTS.md"
            ));
        }
    }
    for (target, event) in &documented {
        if !emitted.contains(&(target.clone(), event.clone())) {
            failures.push(format!(
                "event `{event}` on `{target}` is in docs/EVENTS.md but never emitted"
            ));
        }
    }
    failures
}

/// The `## gatel::x` headings, and the first column of the table under each.
fn documented_events(text: &str) -> BTreeSet<(String, String)> {
    let mut events = BTreeSet::new();
    let mut target = String::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            target = heading.trim().trim_matches('`').to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        if let Some(name) = rest.split('`').next()
            && !target.is_empty()
        {
            events.insert((target.clone(), name.to_string()));
        }
    }
    events
}

/// `target: "..."` followed within a few lines by `event = "..."`, which is the
/// shape every one of them is written in.
fn emitted_events(root: &Path) -> BTreeSet<(String, String)> {
    let mut events = BTreeSet::new();
    for path in rust_files(root) {
        // The checker is not part of the running gateway, and its own examples
        // of the shape it looks for are not events.
        if path.starts_with(root.join("xtask")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let mut target: Option<String> = None;
        let mut at = 0usize;
        for (index, line) in lines.iter().enumerate() {
            if let Some(found) = quoted_after(line, "target:") {
                target = Some(found);
                at = index;
            }
            if let Some(name) = quoted_after(line, "event =")
                && let Some(target) = &target
                && index.saturating_sub(at) <= 8
            {
                events.insert((target.clone(), name));
            }
        }
    }
    events
}

/// The first double-quoted string after a marker on one line.
fn quoted_after(line: &str, marker: &str) -> Option<String> {
    let rest = line.split_once(marker)?.1;
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].to_string())
}
