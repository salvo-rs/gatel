use super::*;

fn package(name: &str, publish: Value, dependencies: Vec<Value>) -> Value {
    json!({"id": name, "name": name, "version": "0.1.0", "manifest_path": format!("/{name}/Cargo.toml"), "publish": publish, "dependencies": dependencies})
}

fn dependency(name: &str, kind: Value) -> Value {
    json!({"name": name, "rename": "alias", "kind": kind, "path": format!("/{name}"), "req":"^0.1.0", "optional":true, "target":"cfg(windows)"})
}

fn metadata(rows: Vec<Value>) -> Vec<u8> {
    let members: Vec<_> = rows.iter().map(|row| row["id"].clone()).collect();
    serde_json::to_vec(&json!({"workspace_members": members, "packages": rows})).unwrap()
}

#[test]
fn plans_all_publishable_members_with_optional_target_alias_build_and_dev_edges() {
    let packages = parse(&metadata(vec![
        package(
            "host",
            Value::Null,
            vec![
                dependency("plugin", Value::Null),
                dependency("codegen", json!("build")),
                dependency("fixtures", json!("dev")),
            ],
        ),
        package("plugin", Value::Null, vec![dependency("api", Value::Null)]),
        package("api", Value::Null, vec![]),
        package("codegen", Value::Null, vec![]),
        package("fixtures", Value::Null, vec![]),
        package("tooling", json!([]), vec![]),
    ]))
    .unwrap();
    let order = order(&packages).unwrap();
    assert_eq!(order.len(), 5);
    assert!(!order.iter().any(|name| name == "tooling"));
    let position = |name| order.iter().position(|item| item == name).unwrap();
    assert!(position("api") < position("plugin"));
    for dependency in ["plugin", "codegen", "fixtures"] {
        assert!(position(dependency) < position("host"));
    }
}

#[test]
fn incomplete_unpublished_or_cyclic_graphs_fail_before_any_plan_is_emitted() {
    for rows in [
        vec![package(
            "host",
            Value::Null,
            vec![dependency("absent", Value::Null)],
        )],
        vec![
            package("host", Value::Null, vec![dependency("private", Value::Null)]),
            package("private", json!([]), vec![]),
        ],
        vec![
            package("a", Value::Null, vec![dependency("b", Value::Null)]),
            package("b", Value::Null, vec![dependency("a", json!("build"))]),
        ],
    ] {
        assert!(order(&parse(&metadata(rows)).unwrap()).is_err());
    }
}

#[test]
fn registry_policy_and_unversioned_paths_are_not_silently_published() {
    let packages = parse(&metadata(vec![
        package("public", json!(["crates-io"]), vec![]),
        package("private", json!(["internal"]), vec![]),
    ]))
    .unwrap();
    assert_eq!(order(&packages).unwrap(), ["public"]);
    let mut local = dependency("unversioned", Value::Null);
    local["req"] = json!("*");
    assert!(parse(&metadata(vec![package("host", Value::Null, vec![local])])).is_err());
    assert!(parse(b"{}").is_err());
    let mut value: Value =
        serde_json::from_slice(&metadata(vec![package("host", Value::Null, vec![])])).unwrap();
    value["workspace_members"] = json!(["host", "missing"]);
    assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn real_workspace_plan_covers_every_local_release_dependency() {
    let (packages, ordered) = read(&crate::repository_root().unwrap()).unwrap();
    let positions: BTreeMap<_, _> = ordered
        .iter()
        .enumerate()
        .map(|(position, name)| (name.clone(), position))
        .collect();
    assert_eq!(
        ordered.len(),
        packages.values().filter(|package| package.publish).count()
    );
    for name in [
        "gatel-password",
        "gatel-plugin-api",
        "gatel-plugin-runtime",
        "gatel-cgi-http",
        "gatel-cgi-protocol",
        "gatel-process-windows",
        "gatel-cgi",
        "gatel-scgi",
        "gatel-fastcgi",
        "gatel-decode",
        "gatel-maintenance",
    ] {
        assert!(positions.contains_key(name), "omitted {name}");
    }
    for (name, package) in packages.iter().filter(|(_, package)| package.publish) {
        for dependency in &package.dependencies {
            assert!(
                positions[dependency] < positions[name],
                "{dependency} must precede {name}"
            );
        }
    }
}

#[test]
fn cargo_packages_and_verifies_a_complete_isolated_workspace_without_publishing() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = Fixture(std::env::temp_dir().join(format!(
            "gatel-publish-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir_all(&directory.0).unwrap();
    std::fs::write(
        directory.0.join("Cargo.toml"),
        "[workspace]\nresolver='3'\nmembers=['host','plugin','api']\ndefault-members=['host']\n",
    )
    .unwrap();
    for (name, dependencies, source) in [
        ("api", "", "pub fn answer() -> u32 { 42 }"),
        (
            "plugin",
            "[dependencies]\nrelease-fixture-api={version='0.1.0',path='../api'}\n",
            "pub fn answer() -> u32 { release_fixture_api::answer() }",
        ),
        (
            "host",
            "[dependencies]\nrelease-fixture-plugin={version='0.1.0',path='../plugin'}\n",
            "pub fn answer() -> u32 { release_fixture_plugin::answer() }",
        ),
    ] {
        std::fs::create_dir_all(directory.0.join(name).join("src")).unwrap();
        std::fs::write(directory.0.join(name).join("Cargo.toml"), format!("[package]\nname='release-fixture-{name}'\nversion='0.1.0'\nedition='2024'\nlicense='Apache-2.0'\n{dependencies}")).unwrap();
        std::fs::write(directory.0.join(name).join("src/lib.rs"), source).unwrap();
    }
    let (_, ordered) = read(&directory.0).unwrap();
    assert_eq!(
        ordered,
        [
            "release-fixture-api",
            "release-fixture-plugin",
            "release-fixture-host"
        ]
    );
    let target = directory.0.join("package-target");
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(&directory.0)
        .env("CARGO_TARGET_DIR", &target)
        .args(["package", "--workspace", "--allow-dirty", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for package in ordered {
        assert!(
            target
                .join("package")
                .join(format!("{package}-0.1.0.crate"))
                .is_file()
        );
    }
}
