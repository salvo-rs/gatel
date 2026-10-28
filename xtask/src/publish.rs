//! Release order comes from Cargo's declarations, including optional/target edges.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};

#[derive(Debug)]
struct Package {
    version: String,
    manifest: PathBuf,
    publish: bool,
    dependencies: BTreeSet<String>,
}

pub(crate) fn run(root: &Path, args: &[String]) -> ExitCode {
    if !args.is_empty() && args != ["--json"] {
        eprintln!("xtask: usage: publish-plan [--json]");
        return ExitCode::from(2);
    }
    match read(root) {
        Ok((packages, order)) => {
            if args.is_empty() {
                for name in order {
                    println!("{name}");
                }
            } else {
                let rows: Vec<_> = order
                    .into_iter()
                    .map(|name| {
                        json!({
                            "version": packages[&name].version,
                            "manifest": packages[&name].manifest,
                            "name": name,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).expect("JSON values serialize")
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read(root: &Path) -> Result<(BTreeMap<String, Package>, Vec<String>), String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("cannot read publish metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "publish metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let packages = parse(&output.stdout)?;
    let order = order(&packages)?;
    Ok((packages, order))
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, Package>, String> {
    let metadata: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid publish metadata: {error}"))?;
    let members = metadata["workspace_members"]
        .as_array()
        .ok_or("missing workspace members")?;
    let rows = metadata["packages"]
        .as_array()
        .ok_or("missing workspace packages")?;
    let mut packages = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        if !members.contains(&row["id"]) {
            continue;
        }
        let name = row["name"].as_str().ok_or("missing publish package name")?;
        let version = row["version"].as_str().ok_or("missing publish package version")?;
        let manifest = row["manifest_path"]
            .as_str()
            .ok_or("missing publish package manifest")?;
        let publish = match row.get("publish") {
            Some(Value::Null) => true,
            Some(Value::Array(registries)) if registries.iter().all(Value::is_string) => {
                registries.iter().any(|registry| registry == "crates-io")
            }
            _ => return Err(format!("invalid publish policy for {name}")),
        };
        let mut dependencies = BTreeSet::new();
        for dependency in row["dependencies"]
            .as_array()
            .ok_or("missing publish dependencies")?
        {
            if !publish {
                continue;
            }
            if dependency["path"].is_null() {
                continue;
            }
            let kind = dependency.get("kind").ok_or("missing dependency kind")?;
            if !kind.is_null() && !matches!(kind.as_str(), Some("build" | "dev")) {
                return Err(format!("unknown dependency kind for {name}"));
            }
            // Cargo omits unversioned dev path dependencies from packages.
            if kind == "dev" && dependency["req"] == "*" {
                continue;
            }
            if dependency["req"]
                .as_str()
                .is_none_or(|requirement| requirement == "*")
            {
                return Err(format!(
                    "{name} has a path dependency without a bounded release version"
                ));
            }
            let target = dependency["name"]
                .as_str()
                .ok_or("missing internal dependency name")?;
            dependencies.insert(target.to_owned());
        }
        if !seen.insert(row["id"].to_string())
            || packages
                .insert(
                    name.to_owned(),
                    Package {
                        version: version.to_owned(),
                        manifest: PathBuf::from(manifest),
                        publish,
                        dependencies,
                    },
                )
                .is_some()
        {
            return Err(format!("duplicate publish package: {name}"));
        }
    }
    if seen.len() != members.len() {
        return Err("workspace members missing from publish metadata".into());
    }
    Ok(packages)
}

fn order(packages: &BTreeMap<String, Package>) -> Result<Vec<String>, String> {
    let mut prerequisites = BTreeMap::new();
    for (name, package) in packages.iter().filter(|(_, package)| package.publish) {
        for dependency in &package.dependencies {
            let target = packages
                .get(dependency)
                .ok_or_else(|| format!("{name} depends on missing workspace package {dependency}"))?;
            if !target.publish {
                return Err(format!(
                    "{name} depends on {dependency}, which cannot publish to crates.io"
                ));
            }
        }
        prerequisites.insert(name.clone(), package.dependencies.clone());
    }
    let mut ready: BTreeSet<_> = prerequisites
        .iter()
        .filter(|(_, dependencies)| dependencies.is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    let mut ordered = Vec::with_capacity(prerequisites.len());
    while let Some(name) = ready.pop_first() {
        prerequisites.remove(&name);
        for (dependent, dependencies) in &mut prerequisites {
            if dependencies.remove(&name) && dependencies.is_empty() {
                ready.insert(dependent.clone());
            }
        }
        ordered.push(name);
    }
    if !prerequisites.is_empty() {
        return Err(format!(
            "publication dependency cycle involving: {}",
            prerequisites.into_keys().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(ordered)
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
