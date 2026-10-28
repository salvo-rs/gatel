//! Log and quota policies must either commit completely or leave live state alone.
mod support;
use std::sync::Arc;
use std::time::Duration;

use gatel_model::{Snapshot, SourceId, SourceRev};
use gatel_plan::{BindingChange, Outcome, StateKind};
use support::*;

async fn key(gateway: &Gateway, value: &str) -> String {
    request(
        gateway.address(),
        &format!("GET / HTTP/1.1\r\nHost: localhost\r\nX-Key: {value}\r\nConnection: close\r\n\r\n"),
    )
    .await
}

#[tokio::test]
async fn max_keys_changes_reset_explicitly_and_failed_candidates_keep_live_counts() {
    let gateway = Gateway::new("quota-capacity");
    let port = free_port();
    let config = |max_keys, terminal: &str| {
        format!(
            "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/\" id=\"quota\" {{ rate-limit key=\"header:x-key\" max=1 window=\"1h\" max-keys={max_keys}; {terminal}; }} }}"
        )
    };
    assert!(matches!(
        gateway.apply(&config(2, "respond \"ok\"")).await,
        Outcome::Started { .. }
    ));
    assert!(key(&gateway, "a").await.starts_with("HTTP/1.1 200"));
    assert!(key(&gateway, "a").await.starts_with("HTTP/1.1 429"));
    let before = gateway.store.current().unwrap();
    let binding = before
        .plan
        .bindings
        .iter()
        .find(|binding| binding.kind == StateKind::Directive("rate-limit"))
        .unwrap();
    let old = gateway
        .state
        .get::<gatel_bundle_std::rate_limit::Buckets>(&binding.owner, binding.kind)
        .unwrap();
    let failed = gateway
        .apply(&config(10, "file-server root=\"absent-root\""))
        .await;
    assert!(matches!(failed, Outcome::PrepareFailed(_)), "{failed:?}");
    assert!(key(&gateway, "a").await.starts_with("HTTP/1.1 429"));
    assert!(matches!(
        gateway.apply(&config(2, "respond \"ok\"")).await,
        Outcome::NoOp { .. }
    ));
    let outcome = gateway.apply(&config(10, "respond \"ok\"")).await;
    let Outcome::Reloaded { changes, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert!(changes.iter().any(|change| matches!(change, BindingChange::Replace { binding, reason: "incompatible change" } if binding.kind == StateKind::Directive("rate-limit"))));
    assert!(
        key(&gateway, "a").await.starts_with("HTTP/1.1 200"),
        "capacity change explicitly resets counters"
    );
    for value in ["b", "c", "d", "e", "f", "g", "h", "i", "j"] {
        assert!(key(&gateway, value).await.starts_with("HTTP/1.1 200"));
    }
    let raised = gateway
        .state
        .get::<gatel_bundle_std::rate_limit::Buckets>(&binding.owner, binding.kind)
        .unwrap();
    assert!(!Arc::ptr_eq(&old, &raised));
    assert_eq!(raised.tracked(), 10);
    assert!(
        key(&gateway, "a").await.starts_with("HTTP/1.1 429"),
        "new keys did not clear the old two-key table"
    );
    let outcome = gateway.apply(&config(2, "respond \"ok\"")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }));
    let smaller = gateway
        .state
        .get::<gatel_bundle_std::rate_limit::Buckets>(&binding.owner, binding.kind)
        .unwrap();
    assert!(!Arc::ptr_eq(&raised, &smaller));
    assert_eq!(smaller.tracked(), 0);
    for value in ["a", "b", "c"] {
        key(&gateway, value).await;
    }
    assert!(smaller.tracked() <= 2);
    gateway.stop().await;
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn log_config(port: u16, rotate: u64, keep: u32, terminal: &str) -> String {
    format!(
        "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ access-log output=\"file:access.log\" rotate={rotate} keep={keep}; route \"/\" {{ {terminal}; }} }}"
    )
}

#[tokio::test]
async fn failed_log_policy_and_same_generation_conflict_do_not_change_live_writer() {
    let gateway = Gateway::new("log-policy");
    let port = free_port();
    let path = gateway.directory.join("access.log");
    assert!(matches!(
        gateway.apply(&log_config(port, 0, 0, "respond \"old\"")).await,
        Outcome::Started { .. }
    ));
    get(gateway.address(), "/").await;
    eventually(|| std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0)).await;
    let original = gateway.telemetry.sinks.snapshot()[0].clone();
    let failed = gateway
        .apply(&log_config(port, 1, 1, "file-server root=\"absent-root\""))
        .await;
    assert!(matches!(failed, Outcome::PrepareFailed(_)), "{failed:?}");
    let before = std::fs::metadata(&path).unwrap().len();
    get(gateway.address(), "/").await;
    eventually(|| std::fs::metadata(&path).is_ok_and(|meta| meta.len() > before)).await;
    assert!(!path.with_extension("log.1").exists());
    let conflict = format!(
        "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ access-log output=\"file:access.log\" rotate=1 keep=1; route \"/\" {{ access-log output=\"file:./access.log\" rotate=0; respond \"new\"; }} }}"
    );
    let failed = gateway.apply(&conflict).await;
    assert!(
        matches!(failed, Outcome::PrepareFailed(ref error) if error.contains("conflicting rotation")),
        "{failed:?}"
    );
    let before = std::fs::metadata(&path).unwrap().len();
    assert!(get(gateway.address(), "/").await.ends_with("old"));
    eventually(|| std::fs::metadata(&path).is_ok_and(|meta| meta.len() > before)).await;
    assert!(!path.with_extension("log.1").exists());
    assert!(matches!(
        gateway.apply(&log_config(port, 1, 2, "respond \"new\"")).await,
        Outcome::Reloaded { .. }
    ));
    assert!(Arc::ptr_eq(&original, &gateway.telemetry.sinks.snapshot()[0]));
    get(gateway.address(), "/").await;
    eventually(|| path.with_extension("log.1").exists()).await;
    get(gateway.address(), "/").await;
    eventually(|| path.with_extension("log.2").exists()).await;
    gateway.apply(&log_config(port, 1, 1, "respond \"new\"")).await;
    get(gateway.address(), "/").await;
    eventually(|| !path.with_extension("log.2").exists()).await;
    gateway.stop().await;
}

fn snapshot(gateway: &Gateway, text: &str, revision: u64) -> Snapshot {
    let path = gateway.directory.join("candidate.kdl");
    std::fs::write(&path, text).unwrap();
    let mut source = gatel_kdl::snapshot(&path, SourceId::file("main"), None).unwrap();
    source.rev = SourceRev(revision);
    Snapshot::new().with(source)
}
fn prepare(
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
            acme: &|_| Err("no ACME".into()),
            telemetry: &gateway.telemetry,
            root: &gateway.directory,
            internal_ca: gateway.directory.join("ca"),
        },
    )
}
#[tokio::test]
async fn a_stale_log_candidate_cannot_replace_the_winners_policy() {
    let gateway = Gateway::new("log-stale");
    let port = free_port();
    gateway.apply(&log_config(port, 0, 0, "respond \"old\"")).await;
    let candidate = snapshot(&gateway, &log_config(port, 1, 2, "respond \"loser\""), 2);
    let winner = snapshot(&gateway, &log_config(port, 0, 0, "respond \"winner\""), 3);
    let outcome = gateway
        .store
        .attempt(&candidate, &gateway.registry, |preparation| {
            let candidate = prepare(&gateway, preparation)?;
            let outcome = gateway.store.attempt(&winner, &gateway.registry, |preparation| {
                prepare(&gateway, preparation)
            });
            assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
            Ok(candidate)
        });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    gateway.server.sync().await;
    assert!(get(gateway.address(), "/").await.ends_with("winner"));
    let path = gateway.directory.join("access.log");
    eventually(|| std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0)).await;
    assert!(!path.with_extension("log.1").exists());
    gateway.stop().await;
}
