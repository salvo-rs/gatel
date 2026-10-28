//! Check the build inventory and local evidence links in the threat model.
//!
//! This verifies source/catalog agreement, not that a named test proves a
//! security property. Changes to the documented guarantees still need review.
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const DOC: &str = "docs/design/threat-model.md";
const UNSAFE_BOUNDARY: &str = "gatel-process-windows";

pub(crate) fn check(root: &Path) -> Vec<String> {
    match inspect(root) {
        Ok(failures) => failures,
        Err(error) => vec![format!("threat-model contract inventory: {error}")],
    }
}

fn inspect(root: &Path) -> Result<Vec<String>, String> {
    let doc = std::fs::read_to_string(root.join(DOC)).map_err(|e| e.to_string())?;
    let workflow =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).map_err(|e| e.to_string())?;
    let fuzz = metadata(root, Some("fuzz/Cargo.toml"))?;
    let workspace = metadata(root, None)?;
    let mut failures = inventories(&doc, &workflow, &fuzz)?;
    failures.extend(production_roots(root, &workspace, &doc)?);
    failures.extend(evidence(root, &root.join(DOC), &doc));
    Ok(failures)
}

fn metadata(root: &Path, manifest: Option<&str>) -> Result<Value, String> {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root).args([
        "metadata",
        "--offline",
        "--locked",
        "--no-deps",
        "--format-version",
        "1",
    ]);
    if let Some(manifest) = manifest {
        command.args(["--manifest-path", manifest]);
    }
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("invalid cargo metadata: {e}"))
}

fn quoted_inventory(doc: &str, prefix: &str) -> Result<BTreeSet<String>, String> {
    let lines: Vec<_> = doc.lines().filter(|line| line.starts_with(prefix)).collect();
    if lines.len() != 1 {
        return Err(format!("expected exactly one `{prefix}` inventory line"));
    }
    if lines[0].matches('`').count() % 2 != 0 {
        return Err(format!("unpaired backtick in `{prefix}` inventory"));
    }
    let names: Vec<_> = lines[0]
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    let distinct: BTreeSet<_> = names.iter().cloned().collect();
    if distinct.len() != names.len() {
        return Err(format!("duplicate `{prefix}` inventory item"));
    }
    if distinct.is_empty() {
        return Err(format!("empty `{prefix}` inventory"));
    }
    Ok(distinct)
}

fn ci_targets(workflow: &str) -> Result<BTreeSet<String>, String> {
    let mut lines = workflow.lines().skip_while(|line| *line != "  fuzz:");
    lines.next().ok_or("missing CI fuzz job")?;
    // The repository keeps this inventory as one inline YAML sequence. Fail
    // closed if its shape changes rather than silently checking a subset.
    let list = lines
        .take_while(|line| !line.starts_with("  ") || line.starts_with("   "))
        .find_map(|line| line.trim().strip_prefix("target:"))
        .ok_or("missing CI fuzz target matrix")?
        .trim();
    let list = list
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or("CI fuzz target matrix must be an inline sequence")?;
    let names: BTreeSet<_> = list
        .split(',')
        .map(|name| name.trim().trim_matches(['\'', '"']).to_string())
        .collect();
    if names.is_empty() || names.iter().any(String::is_empty) {
        return Err("empty CI fuzz target matrix".into());
    }
    Ok(names)
}

fn inventories(doc: &str, workflow: &str, fuzz: &Value) -> Result<Vec<String>, String> {
    let mut failures = Vec::new();
    let packages = fuzz
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("missing fuzz metadata packages")?;
    let package = packages
        .iter()
        .find(|package| package.get("name").and_then(Value::as_str) == Some("gatel-fuzz"))
        .ok_or("missing gatel-fuzz package")?;
    let targets = package
        .get("targets")
        .and_then(Value::as_array)
        .ok_or("missing fuzz targets")?;
    let mut built = BTreeSet::new();
    for target in targets {
        if target
            .get("kind")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        {
            built.insert(
                target
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("unnamed fuzz binary")?
                    .to_string(),
            );
        }
    }
    if built.is_empty() {
        return Err("no fuzz binary targets".into());
    }
    let ci = ci_targets(workflow)?;
    let documented = quoted_inventory(doc, "CI fuzz targets:")?;
    if built != ci {
        failures.push(format!("fuzz manifest targets {built:?} disagree with CI {ci:?}"));
    }
    if built != documented {
        failures.push(format!(
            "fuzz manifest targets {built:?} disagree with threat model {documented:?}"
        ));
    }
    let budgets = quoted_inventory(doc, "CI fuzz budget:")?;
    let budget = workflow
        .split_once("-max_total_time=")
        .ok_or("missing CI fuzz time budget")?
        .1
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if budget.is_empty() || budgets != BTreeSet::from([budget]) {
        failures.push("CI fuzz time budget disagrees with threat model".into());
    }
    Ok(failures)
}

fn production_roots(root: &Path, metadata: &Value, doc: &str) -> Result<Vec<String>, String> {
    let exceptions = quoted_inventory(doc, "Unsafe production boundary:")?;
    if exceptions != BTreeSet::from([UNSAFE_BOUNDARY.to_string()]) {
        return Err("documented unsafe boundary differs from the checked allowlist".into());
    }
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("missing workspace packages")?;
    let members = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or("missing workspace members")?;
    let mut failures = Vec::new();
    let mut saw_boundary = false;
    let mut checked = 0;
    for package in packages {
        if !members.contains(package.get("id").ok_or("missing workspace package id")?) {
            continue;
        }
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or("unnamed workspace package")?;
        for target in package
            .get("targets")
            .and_then(Value::as_array)
            .ok_or("missing package targets")?
        {
            let kinds = target
                .get("kind")
                .and_then(Value::as_array)
                .ok_or("missing target kind")?;
            if !kinds.iter().any(|kind| {
                matches!(
                    kind.as_str(),
                    Some(
                        "lib"
                            | "proc-macro"
                            | "bin"
                            | "cdylib"
                            | "staticlib"
                            | "rlib"
                            | "dylib"
                            | "custom-build"
                    )
                )
            }) {
                continue;
            }
            let source = target
                .get("src_path")
                .and_then(Value::as_str)
                .ok_or("missing target source path")?;
            let source = root.join(source);
            let text = std::fs::read_to_string(&source)
                .map_err(|e| format!("cannot read {}: {e}", source.display()))?;
            checked += 1;
            let policy = if name == UNSAFE_BOUNDARY {
                saw_boundary = true;
                "#![deny(unsafe_op_in_unsafe_fn)]"
            } else {
                "#![forbid(unsafe_code)]"
            };
            if !root_policy(&text, policy) {
                failures.push(format!(
                    "production target `{name}` at {} lacks {policy}",
                    source.display()
                ));
            }
        }
    }
    if checked == 0 || !saw_boundary {
        return Err("workspace production target inventory is incomplete".into());
    }
    Ok(failures)
}

fn root_policy(source: &str, policy: &str) -> bool {
    // Current crate roots use line comments and single-line inner attributes.
    // Stop at the first item: an attribute quoted in a fixture/string is not
    // a crate policy. A different preamble shape must be explicitly supported.
    for line in source.lines().map(str::trim) {
        if line == policy {
            return true;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#![") {
            continue;
        }
        break;
    }
    false
}

fn evidence(root: &Path, document: &Path, text: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let Ok(root) = root.canonicalize() else {
        return vec!["cannot resolve evidence root".into()];
    };
    for link in text.split("](").skip(1) {
        let Some((link, _)) = link.split_once(')') else {
            failures.push("unterminated threat-model evidence link".into());
            continue;
        };
        if link.starts_with("https://") || link.starts_with("http://") || link.starts_with('#') {
            continue;
        }
        let link = link.split('#').next().unwrap_or_default();
        let path = document.parent().unwrap_or(Path::new(".")).join(link);
        if !path.is_file() || !matches!(path.canonicalize(), Ok(path) if path.starts_with(&root)) {
            failures.push(format!(
                "missing or out-of-workspace threat-model evidence: {link}"
            ));
        }
    }
    failures
}

#[cfg(test)]
#[path = "contracts_tests.rs"]
mod tests;
