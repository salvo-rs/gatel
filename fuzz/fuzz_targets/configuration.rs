//! A whole configuration, from text to a compiled plan.
//!
//! Wider than the frontend on purpose: normalising is where names are
//! resolved, references checked and defaults filled in, and it is reached with
//! whatever nodes the frontend accepted rather than with nodes anyone wrote.

#![no_main]

use std::sync::Arc;

use gatel_model::{Hasher, Snapshot, SourceId, SourceRev, SourceSnapshot};
use gatel_plan::Registry;
use libfuzzer_sys::fuzz_target;

fn registry() -> Registry {
    Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .expect("the standard bundle is well formed")
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(nodes) = gatel_kdl::lower(text, "fuzz.kdl") else {
        return;
    };

    let mut hasher = Hasher::new();
    hasher.update_str(text);
    let snapshot = Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        hasher.finish(),
        nodes,
    ));

    let registry = registry();
    let Ok(spec) = gatel_plan::normalize(&snapshot, &registry) else {
        return;
    };
    // Compiling is where ordering, specificity and state bindings are decided.
    // It is not allowed to fail, so reaching it with anything that normalised
    // is exactly what is worth checking.
    let _ = gatel_plan::compile(Arc::new(spec), &registry);
});
