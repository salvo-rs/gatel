//! Cargo parses manifests, including target tables, aliases and workspace inheritance.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

pub(crate) struct Package {
    pub manifest: PathBuf,
    pub dependencies: Vec<String>,
}

pub(crate) fn read(root: &Path) -> Result<BTreeMap<String, Package>, String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(["metadata", "--offline", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("cannot run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse(&output.stdout)
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, Package>, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid cargo metadata: {e}"))?;
    let packages = value
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("missing metadata packages")?;
    let members = value
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or("missing workspace members")?;
    let mut found = BTreeMap::new();
    for package in packages {
        if !members.contains(package.get("id").ok_or("missing package id")?) {
            continue;
        }
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or("missing package name")?;
        if name == "xtask" {
            continue;
        } // Build tooling is not a gateway layer.
        let manifest = PathBuf::from(
            package
                .get("manifest_path")
                .and_then(Value::as_str)
                .ok_or("missing manifest path")?,
        );
        let mut dependencies = Vec::new();
        for dependency in package
            .get("dependencies")
            .and_then(Value::as_array)
            .ok_or("missing dependency declarations")?
        {
            // All target conditions and optional dependencies count. Only tests
            // are exempt; build-dependencies can also violate architecture.
            match dependency.get("kind") {
                Some(Value::Null) => {}
                Some(Value::String(kind)) if kind == "build" => {}
                Some(Value::String(kind)) if kind == "dev" => continue,
                _ => return Err("unknown dependency kind".into()),
            }
            dependencies.push(
                dependency
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("missing dependency name")?
                    .to_owned(),
            );
        }
        dependencies.sort();
        dependencies.dedup();
        if found
            .insert(
                name.to_owned(),
                Package {
                    manifest,
                    dependencies,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate workspace package name: {name}"));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn real_cargo_manifest_exposes_renamed_optional_target_dependency() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
                "gatel-dependency-check-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        for package in ["app", "backend"] {
            std::fs::create_dir_all(fixture.0.join(package).join("src")).unwrap();
            std::fs::write(fixture.0.join(package).join("src/lib.rs"), "").unwrap();
        }
        std::fs::write(
            fixture.0.join("Cargo.toml"),
            "[workspace]\nresolver = '3'\nmembers = ['app', 'backend']\n",
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("backend/Cargo.toml"),
            "[package]\nname = 'restricted-layer'\nversion = '0.1.0'\nedition = '2024'\n",
        )
        .unwrap();
        std::fs::write(fixture.0.join("app/Cargo.toml"), "[package]\nname = 'app'\nversion = '0.1.0'\nedition = '2024'\n[target.'cfg(unix)'.dependencies.harmless_alias]\npackage = 'restricted-layer'\npath = '../backend'\noptional = true\n").unwrap();
        let packages = read(&fixture.0).unwrap();
        assert_eq!(packages["app"].dependencies, ["restricted-layer"]);
    }
    #[test]
    fn aliases_targets_optional_and_build_dependencies_are_checked() {
        let value = json!({"workspace_members": ["a"], "packages": [{"id":"a", "name":"a", "manifest_path":"a/Cargo.toml", "dependencies": [
            {"name":"forbidden", "rename":"innocent", "kind":null, "target":"cfg(windows)", "optional":true},
            {"name":"compiler", "kind":"build"}, {"name":"test-only", "kind":"dev"}
        ]}]});
        let packages = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(packages["a"].dependencies, ["compiler", "forbidden"]);
    }
    #[test]
    fn invalid_metadata_never_becomes_empty_success() {
        for input in [
            b"{}".as_slice(),
            b"broken",
            b"{\"packages\":[],\"workspace_members\":null}",
        ] {
            assert!(parse(input).is_err());
        }
    }
}
