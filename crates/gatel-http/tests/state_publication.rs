//! Candidate registration changes must not leak through a failed preparation.
mod support;

use std::sync::Arc;

use gatel_model::{ResourceId, ResourceKind};
use gatel_plan::{Outcome, StateKind};
use gatel_state::ConnectionPool;
use support::*;

fn prepare_candidate(
    gateway: &Gateway,
    preparation: gatel_plan::Preparation<'_, Arc<gatel_http::Runtime>>,
) -> Result<Arc<gatel_http::Runtime>, String> {
    gatel_http::prepare(
        &preparation,
        &gatel_http::Resources {
            blocking_budget: None,
            blocking: gateway.server.blocking_executor(),
            plugin_retirements: gateway.server.plugin_retirements(),
            binds: &gateway.binds,
            state: &gateway.state,
            certificates: &gateway.certificates,
            challenges: &gateway.challenges,
            acme: &|_| Err("test does not use ACME".into()),
            telemetry: &gateway.telemetry,
            root: &gateway.directory,
            internal_ca: gateway.directory.join("ca"),
        },
    )
}

#[tokio::test]
async fn stale_candidate_cannot_supply_or_overwrite_the_winners_state() {
    let gateway = Gateway::new("stale-state-publication");
    let port = free_port();
    let initial = format!(
        r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "api" {{ endpoint "127.0.0.1:9000"; }}
site "*" {{ route "/" {{ respond "ok"; }} }}
"#
    );
    assert!(matches!(gateway.apply(&initial).await, Outcome::Started { .. }));
    let path = gateway.directory.join("gatel.kdl");
    std::fs::write(&path, initial.replace(":9000", ":9001")).unwrap();
    let mut source = gatel_kdl::snapshot(&path, gatel_model::SourceId::file("main"), None).unwrap();
    source.rev = gatel_model::SourceRev(2);
    let snapshot = gatel_model::Snapshot::new().with(source);
    let mut candidate_pool = None;
    let outcome = gateway
        .store
        .attempt(&snapshot, &gateway.registry, |preparation| {
            let candidate = prepare_candidate(&gateway, preparation)?;
            candidate_pool = Some(candidate.upstreams.iter().next().unwrap().pool.clone());
            // A later preparation commits before the first finishes.
            let winner = gateway
                .store
                .attempt(&snapshot, &gateway.registry, |preparation| {
                    prepare_candidate(&gateway, preparation)
                });
            assert!(matches!(winner, Outcome::Reloaded { .. }), "{winner:?}");
            Ok(candidate)
        });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    let current = gateway.store.current().unwrap();
    let winner = current.runtime.upstreams.iter().next().unwrap();
    assert!(!Arc::ptr_eq(&candidate_pool.unwrap(), &winner.pool));
    assert!(Arc::ptr_eq(
        &winner.pool,
        &gateway
            .state
            .get::<ConnectionPool>(&winner.spec.id, StateKind::Pool)
            .unwrap()
    ));
    gateway.stop().await;
}

#[tokio::test]
async fn reused_pool_capacity_changes_only_after_successful_publication() {
    let gateway = Gateway::new("pool-policy-publication");
    let port = free_port();
    let address = backend("policy").await;
    let config = |capacity: u32, fail: bool| {
        let auth = if fail {
            "basic-auth { user \"admin\" hash=\"missing.hash\"; }"
        } else {
            ""
        };
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "api" {{ endpoint "{address}"; pool max-per-endpoint={capacity} max-idle-per-endpoint=1; }}
site "*" {{ route "/" {{ {auth}
respond "ok";
}} }}
"#
        )
    };
    let outcome = gateway.apply(&config(1, false)).await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
    let current = gateway.store.current().unwrap();
    let upstream = current.runtime.upstreams.iter().next().unwrap();
    let endpoint = &upstream.endpoints.all()[0];
    let mut timeouts = upstream.spec.timeouts;
    timeouts.queue = std::time::Duration::from_millis(20);
    let first = upstream.pool.checkout(endpoint, &timeouts).await.unwrap();
    assert!(matches!(
        gateway.apply(&config(2, true)).await,
        Outcome::PrepareFailed(_)
    ));
    assert!(matches!(
        upstream.pool.checkout(endpoint, &timeouts).await,
        Err(gatel_state::CheckoutError::QueueTimeout(_))
    ));
    assert!(matches!(
        gateway.apply(&config(2, false)).await,
        Outcome::Reloaded { .. }
    ));
    let next = gateway.store.current().unwrap();
    assert!(Arc::ptr_eq(
        &upstream.pool,
        &next.runtime.upstreams.iter().next().unwrap().pool
    ));
    // The old handle sees the committed capacity without losing its first lease.
    let second = upstream.pool.checkout(endpoint, &timeouts).await.unwrap();
    assert_eq!(upstream.pool.stats()[0].in_use, 2);
    drop(second);
    drop(first);
    gateway.stop().await;
}

#[tokio::test]
async fn failed_candidate_preserves_registry_and_following_reload_reuse() {
    let gateway = Gateway::new("state-publication");
    let port = free_port();
    let config = |backend: u16, auth: bool, body: &str| {
        let auth = if auth {
            "basic-auth { user \"admin\" hash=\"missing.hash\"; }"
        } else {
            ""
        };
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
upstream "api" {{ endpoint "127.0.0.1:{backend}"; }}
site "*" {{ route "/" {{
{auth}
respond "{body}";
}} }}
"#
        )
    };
    let outcome = gateway.apply(&config(9000, false, "original")).await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
    let owner = ResourceId::new(ResourceKind::Upstream, "api");
    let current = gateway.store.current().unwrap();
    let pool = current.runtime.upstreams.get(&owner).unwrap().pool.clone();
    let keys = gateway.state.keys();
    let outcome = gateway.apply(&config(9001, true, "invalid")).await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert!(Arc::ptr_eq(&current, &gateway.store.current().unwrap()));
    assert_eq!(gateway.state.keys(), keys);
    assert!(Arc::ptr_eq(
        &pool,
        &gateway
            .state
            .get::<ConnectionPool>(&owner, StateKind::Pool)
            .unwrap()
    ));
    assert!(get(gateway.address(), "/").await.ends_with("original"));

    let outcome = gateway.apply(&config(9000, false, "next")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    let next = gateway.store.current().unwrap();
    assert!(Arc::ptr_eq(
        &pool,
        &next.runtime.upstreams.get(&owner).unwrap().pool
    ));
    let outcome = gateway.apply(&config(9001, false, "replacement")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    let replacement = gateway
        .store
        .current()
        .unwrap()
        .runtime
        .upstreams
        .get(&owner)
        .unwrap()
        .pool
        .clone();
    assert!(!Arc::ptr_eq(&pool, &replacement));
    assert!(Arc::ptr_eq(
        &replacement,
        &gateway
            .state
            .get::<ConnectionPool>(&owner, StateKind::Pool)
            .unwrap()
    ));
    gateway.stop().await;
}
