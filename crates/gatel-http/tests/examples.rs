//! Every example must still describe a configuration this build accepts.
//!
//! Examples are documentation that runs. One that stopped compiling would be
//! read by somebody, copied, and only then found to be wrong.

use std::path::PathBuf;
use std::sync::Arc;

use gatel_model::{Snapshot, SourceId};
use gatel_plan::Registry;

fn examples() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root")
        .join("examples");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kdl"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_example_compiles() {
    let registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .unwrap();

    let files = examples();
    assert!(files.len() >= 8, "the examples are the documentation that runs");

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source = gatel_kdl::snapshot(&path, SourceId::file("main"), None)
            .unwrap_or_else(|diagnostics| panic!("{name} does not load:\n{diagnostics}"));
        let snapshot = Snapshot::new().with(source);
        let spec = gatel_plan::normalize(&snapshot, &registry)
            .unwrap_or_else(|diagnostics| panic!("{name} is not valid:\n{diagnostics}"));
        let plan = gatel_plan::compile(Arc::new(spec), &registry);
        assert!(
            !plan.listeners.is_empty(),
            "{name} compiles to nothing that serves"
        );
    }
}

#[test]
fn no_example_writes_a_secret_in_place() {
    // An example is copied. One with a password in it is a password in
    // somebody's repository a week later.
    for path in examples() {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with("//") {
                continue;
            }
            for field in ["hash=", "password=", "token=", "secret="] {
                if let Some(rest) = line.split(field).nth(1) {
                    let value = rest.trim_start_matches('"');
                    assert!(
                        value.starts_with("env:") || value.starts_with("file:"),
                        "{name}:{} writes `{field}` in place; it must be a reference",
                        number + 1
                    );
                }
            }
        }
    }
}
