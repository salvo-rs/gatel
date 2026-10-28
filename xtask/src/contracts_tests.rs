use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use super::*;

const DOC_INVENTORY: &str = "Unsafe production boundary: `gatel-process-windows`.\nCI fuzz targets: `a`, `b`.\nCI fuzz budget: `180` seconds per target.\n";
const WORKFLOW: &str = "jobs:\n  test:\n    name: test\n  fuzz:\n    strategy:\n      matrix:\n        target: [a, b]\n    steps:\n      - run: cargo fuzz run ${{ matrix.target }} -- -max_total_time=180\n  soak:\n    name: soak\n";

fn fuzz() -> Value {
    json!({"packages": [{"name": "gatel-fuzz", "targets": [
        {"name": "a", "kind": ["bin"]}, {"name": "b", "kind": ["bin"]}
    ]}]})
}

#[test]
fn manifest_ci_and_document_inventory_must_agree() {
    assert!(inventories(DOC_INVENTORY, WORKFLOW, &fuzz()).unwrap().is_empty());
    let ci = WORKFLOW.replace("target: [a, b]", "target: [a]");
    assert!(
        inventories(DOC_INVENTORY, &ci, &fuzz())
            .unwrap()
            .iter()
            .any(|error| error.contains("disagree with CI"))
    );
    let doc = DOC_INVENTORY.replace("`a`, `b`", "`a`, `obsolete`");
    assert!(
        inventories(&doc, WORKFLOW, &fuzz())
            .unwrap()
            .iter()
            .any(|error| error.contains("disagree with threat model"))
    );
    let budget = WORKFLOW.replace("max_total_time=180", "max_total_time=30");
    assert!(
        inventories(DOC_INVENTORY, &budget, &fuzz())
            .unwrap()
            .iter()
            .any(|error| error.contains("time budget"))
    );
}

#[test]
fn incomplete_or_unsupported_inventory_never_succeeds() {
    assert!(inventories(DOC_INVENTORY, WORKFLOW, &json!({})).is_err());
    assert!(ci_targets("jobs:\n  another:\n    target: [a, b]\n").is_err());
    assert!(ci_targets(&WORKFLOW.replace("[a, b]", "\n          - a\n          - b")).is_err());
    assert!(quoted_inventory("CI fuzz targets: `a`, `a`", "CI fuzz targets:").is_err());
    assert!(quoted_inventory("CI fuzz targets: `a", "CI fuzz targets:").is_err());
    assert!(quoted_inventory("no inventory", "CI fuzz targets:").is_err());
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gatel-contracts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn metadata(&self) -> Value {
        std::fs::write(self.0.join("app.rs"), "#![forbid(unsafe_code)]\nfn main() {}\n").unwrap();
        std::fs::write(self.0.join("ffi.rs"), "#![deny(unsafe_op_in_unsafe_fn)]\n").unwrap();
        json!({"workspace_members": ["app-id", "ffi-id"], "packages": [
            {"id": "app-id", "name": "app", "targets": [{"kind": ["bin"], "src_path": "app.rs"}]},
            {"id": "ffi-id", "name": "gatel-process-windows", "targets": [{"kind": ["lib"], "src_path": "ffi.rs"}]}
        ]})
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn production_roots_enforce_checked_unsafe_boundary() {
    let fixture = Fixture::new();
    let metadata = fixture.metadata();
    assert!(
        production_roots(&fixture.0, &metadata, DOC_INVENTORY)
            .unwrap()
            .is_empty()
    );
    std::fs::write(fixture.0.join("app.rs"), "fn main() {}\n").unwrap();
    assert!(
        production_roots(&fixture.0, &metadata, DOC_INVENTORY)
            .unwrap()
            .iter()
            .any(|error| error.contains("forbid(unsafe_code)"))
    );
    std::fs::write(
        fixture.0.join("app.rs"),
        "const FIXTURE: &str = r#\"\n#![forbid(unsafe_code)]\n\"#;\nfn main() {}\n",
    )
    .unwrap();
    assert!(
        production_roots(&fixture.0, &metadata, DOC_INVENTORY)
            .unwrap()
            .iter()
            .any(|error| error.contains("forbid(unsafe_code)"))
    );
    std::fs::write(fixture.0.join("ffi.rs"), "").unwrap();
    assert!(
        production_roots(&fixture.0, &metadata, DOC_INVENTORY)
            .unwrap()
            .iter()
            .any(|error| error.contains("deny(unsafe_op_in_unsafe_fn)"))
    );
    assert!(
        production_roots(
            &fixture.0,
            &metadata,
            &DOC_INVENTORY.replace("gatel-process-windows", "another-boundary")
        )
        .is_err()
    );
}

#[test]
fn local_evidence_must_exist_and_remain_inside_workspace() {
    let fixture = Fixture::new();
    let document = fixture.0.join("model.md");
    std::fs::write(fixture.0.join("test.rs"), "test evidence").unwrap();
    assert!(
        evidence(
            &fixture.0,
            &document,
            "[test](test.rs#case) [source](https://example.test/proof)"
        )
        .is_empty()
    );
    assert!(!evidence(&fixture.0, &document, "[test](missing.rs)").is_empty());
    assert!(!evidence(&fixture.0, &document, "[test](../outside.rs)").is_empty());
    assert!(!evidence(&fixture.0, &document, "[test](unterminated").is_empty());
}
