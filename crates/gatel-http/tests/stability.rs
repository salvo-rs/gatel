//! The reload stability matrix.
//!
//! `docs/ARCHITECTURE.md` prints a table of what a reload keeps and what it
//! replaces. Until this existed the table was a claim; now a change that breaks
//! a cell fails here.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use gatel_model::{Snapshot, SourceId};
use gatel_plan::{BindingChange, Plan, Registry, StateKind, diff_bindings};

fn registry() -> Registry {
    Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .unwrap()
}

/// A directory no other call to this function can be using.
///
/// It used to be named after the current nanosecond, which is not a unique
/// number: the clock is coarser than that, and these tests run in parallel. A
/// collision meant one test writing its configuration over another's and then
/// deleting the directory underneath it — so a test compiled somebody else's
/// input and failed on an assertion about its own, once every few hundred
/// runs. A counter cannot collide.
fn scratch() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "gatel-stability-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn plan(text: &str) -> Arc<Plan> {
    let directory = scratch();
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("gatel.kdl");
    std::fs::write(&path, text).unwrap();
    let registry = registry();
    let source = gatel_kdl::snapshot(&path, SourceId::file("main"), None)
        .unwrap_or_else(|diagnostics| panic!("{diagnostics}"));
    let snapshot = Snapshot::new().with(source);
    let spec =
        gatel_plan::normalize(&snapshot, &registry).unwrap_or_else(|diagnostics| panic!("{diagnostics}"));
    let _ = std::fs::remove_dir_all(&directory);
    Arc::new(gatel_plan::compile(Arc::new(spec), &registry))
}

/// What a reload from one configuration to another does to each kind of state.
fn transition(before: &str, after: &str) -> BTreeMap<StateKind, &'static str> {
    let changes = diff_bindings(&plan(before).bindings, &plan(after).bindings);
    let mut outcome = BTreeMap::new();
    for change in changes {
        let (kind, verdict) = match change {
            BindingChange::Reuse(binding) => (binding.kind, "reuse"),
            BindingChange::Replace { binding, .. } => (binding.kind, "replace"),
            BindingChange::Create(binding) => (binding.kind, "create"),
            BindingChange::Retire(binding) => (binding.kind, "retire"),
        };
        // Replacing anything of a kind is the interesting answer, so it wins
        // over a sibling that was reused.
        let existing = outcome.entry(kind).or_insert(verdict);
        if verdict != "reuse" {
            *existing = verdict;
        }
    }
    outcome
}

fn config(endpoints: &str, balance: &str, pool: u32, limit: &str) -> String {
    format!(
        r#"
global {{ http "127.0.0.1:1" }}
upstream "api" {{
{endpoints}
    balance "{balance}"
    pool max-per-endpoint={pool}
}}
site "*" id="all" {{
    route "/api/*" id="api" {{
        proxy upstream="api"
        rate-limit {limit}
    }}
    route "/other" id="other" {{ respond "unchanged" }}
}}
"#
    )
}

const TWO: &str = "    endpoint \"10.0.0.1:8080\"\n    endpoint \"10.0.0.2:8080\"";
const OTHER_TWO: &str = "    endpoint \"10.0.9.1:8080\"\n    endpoint \"10.0.9.2:8080\"";
const LIMIT: &str = r#"max=100 window="1m" key="ip""#;

fn base() -> String {
    config(TWO, "round-robin", 8, LIMIT)
}

fn assert_row(name: &str, after: String, expected: &[(StateKind, &str)]) {
    let outcome = transition(&base(), &after);
    for (kind, verdict) in expected {
        assert_eq!(
            outcome.get(kind).copied(),
            Some(*verdict),
            "{name}: {kind:?} should {verdict}, the whole row was {outcome:?}"
        );
    }
}

#[test]
fn raising_a_pool_limit_keeps_everything() {
    // Raising a limit must not drop live connections.
    assert_row(
        "raise max-per-endpoint",
        config(TWO, "round-robin", 32, LIMIT),
        &[
            (StateKind::Endpoints, "reuse"),
            (StateKind::Pool, "reuse"),
            (StateKind::Selector, "reuse"),
            (StateKind::RateLimit, "reuse"),
        ],
    );
}

#[test]
fn replacing_the_backends_replaces_what_points_at_them() {
    // The pool holds connections to addresses that are no longer configured,
    // and the selector's cursor refers to a set that no longer exists.
    assert_row(
        "replace the endpoints",
        config(OTHER_TWO, "round-robin", 8, LIMIT),
        &[
            (StateKind::Endpoints, "replace"),
            (StateKind::Pool, "replace"),
            (StateKind::Selector, "replace"),
            (StateKind::RateLimit, "reuse"),
        ],
    );
}

#[test]
fn changing_the_balance_replaces_only_the_selector() {
    // A cursor into a round robin means nothing to a least-connections
    // balancer, but the connections themselves are still good.
    assert_row(
        "change balance",
        config(TWO, "least-conn", 8, LIMIT),
        &[
            (StateKind::Endpoints, "reuse"),
            (StateKind::Pool, "reuse"),
            (StateKind::Selector, "replace"),
            (StateKind::RateLimit, "reuse"),
        ],
    );
}

#[test]
fn raising_a_rate_limit_does_not_hand_every_client_a_fresh_allowance() {
    assert_row(
        "raise the rate limit",
        config(TWO, "round-robin", 8, r#"max=1000 window="1m" key="ip""#),
        &[(StateKind::RateLimit, "reuse"), (StateKind::Endpoints, "reuse")],
    );
}

#[test]
fn changing_what_a_rate_limit_counts_replaces_its_buckets() {
    // Buckets keyed by address mean nothing once the key is an API key.
    assert_row(
        "change the rate limit key",
        config(
            TWO,
            "round-robin",
            8,
            r#"max=100 window="1m" key="header:x-api-key""#,
        ),
        &[
            (StateKind::RateLimit, "replace"),
            (StateKind::Endpoints, "reuse"),
            (StateKind::Pool, "reuse"),
            (StateKind::Selector, "reuse"),
        ],
    );
}

#[test]
fn editing_an_unrelated_route_keeps_everything() {
    let after = base().replace("unchanged", "edited");
    assert_row(
        "edit an unrelated route",
        after,
        &[
            (StateKind::Endpoints, "reuse"),
            (StateKind::Pool, "reuse"),
            (StateKind::Selector, "reuse"),
            (StateKind::RateLimit, "reuse"),
        ],
    );
}

#[test]
fn removing_an_upstream_retires_its_state() {
    let after = r#"
global { http "127.0.0.1:1" }
site "*" id="all" {
    route "/api/*" id="api" { respond "no backend now" }
    route "/other" id="other" { respond "unchanged" }
}
"#;
    let outcome = transition(&base(), after);
    for kind in [StateKind::Endpoints, StateKind::Pool, StateKind::Selector] {
        assert_eq!(
            outcome.get(&kind).copied(),
            Some("retire"),
            "{kind:?} should be retired, the whole row was {outcome:?}"
        );
    }
}

#[test]
fn moving_a_directive_to_another_route_gives_it_different_state() {
    // State is owned by the route it was written on, so there is no path to a
    // process-wide bucket that follows a directive around.
    let after = base()
        .replace("        rate-limit max=100 window=\"1m\" key=\"ip\"\n", "")
        .replace(
            r#"route "/other" id="other" { respond "unchanged" }"#,
            "route \"/other\" id=\"other\" { respond \"unchanged\"\n rate-limit max=100 window=\"1m\" key=\"ip\" }",
        );
    let outcome = transition(&base(), &after);
    assert_eq!(
        outcome.get(&StateKind::RateLimit).copied(),
        Some("retire"),
        "the buckets belonged to the route it left: {outcome:?}"
    );
}
