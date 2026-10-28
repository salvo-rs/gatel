//! New listeners belong to a candidate until its generation is published.
mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use gatel_model::{Snapshot, SourceId, SourceRev};
use gatel_plan::Outcome;
use support::*;

fn config(addresses: &[SocketAddr], terminal: &str) -> String {
    let listeners: String = addresses
        .iter()
        .enumerate()
        .map(|(index, address)| format!("http \"listener-{index}\" \"{address}\";\n"))
        .collect();
    format!("global {{ {listeners} }}\nsite \"*\" {{ route \"/\" {{ {terminal}; }} }}")
}

fn address() -> SocketAddr {
    ([127, 0, 0, 1], free_port()).into()
}

fn snapshot(gateway: &Gateway, text: &str, revision: u64) -> Snapshot {
    let path = gateway.directory.join("gatel.kdl");
    std::fs::write(&path, text).unwrap();
    let mut source = gatel_kdl::snapshot(&path, SourceId::file("main"), None).unwrap();
    source.rev = SourceRev(revision);
    Snapshot::new().with(source)
}

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
async fn failed_preparation_and_noop_restore_release_only_candidate_ports() {
    let gateway = Gateway::new("bind-failure-noop");
    let active = address();
    let added = address();
    let initial = config(&[active], "respond \"original\"");
    assert!(matches!(gateway.apply(&initial).await, Outcome::Started { .. }));
    let original = gateway.binds.get(&active).unwrap();
    // An embedding host may register another socket independently. Failure
    // must never delete that socket, even if the candidate does not name it.
    let adopted = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let adopted = gateway.binds.adopt(adopted).unwrap();
    let inherited = gateway.binds.get(&adopted).unwrap();

    let outcome = gateway
        .apply(&config(&[active, added], "file-server root=\"missing-root\""))
        .await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert!(gateway.binds.get(&added).is_none());
    assert!(std::net::TcpListener::bind(added).is_ok());
    assert!(Arc::ptr_eq(&original, &gateway.binds.get(&active).unwrap()));
    assert!(Arc::ptr_eq(&inherited, &gateway.binds.get(&adopted).unwrap()));
    assert!(matches!(gateway.apply(&initial).await, Outcome::NoOp { .. }));
    assert!(std::net::TcpListener::bind(added).is_ok());
    assert!(Arc::ptr_eq(&inherited, &gateway.binds.get(&adopted).unwrap()));
    assert!(get(active, "/").await.ends_with("original"));
    gateway.stop().await;
}

#[tokio::test]
async fn stale_candidate_releases_private_ports_and_preserves_shared_winner() {
    let gateway = Gateway::new("stale-bind-publication");
    let active = address();
    let shared = address();
    let discarded = address();
    let added = address();
    let initial = config(&[active], "respond \"original\"");
    assert!(matches!(gateway.apply(&initial).await, Outcome::Started { .. }));
    let original = gateway.binds.get(&active).unwrap();
    let candidate = snapshot(
        &gateway,
        &config(&[active, shared, discarded], "respond \"loser\""),
        2,
    );
    let winner = snapshot(
        &gateway,
        &config(&[active, shared, added], "respond \"winner\""),
        3,
    );
    let outcome = gateway
        .store
        .attempt(&candidate, &gateway.registry, |preparation| {
            let candidate = prepare_candidate(&gateway, preparation)?;
            assert_eq!(gateway.binds.bound(), vec![active]);
            let outcome = gateway.store.attempt(&winner, &gateway.registry, |preparation| {
                prepare_candidate(&gateway, preparation)
            });
            assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
            Ok(candidate)
        });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    assert!(std::net::TcpListener::bind(discarded).is_ok());
    assert!(gateway.binds.get(&discarded).is_none());
    assert!(gateway.binds.get(&shared).is_some());
    assert!(gateway.binds.get(&added).is_some());
    assert!(Arc::ptr_eq(&original, &gateway.binds.get(&active).unwrap()));
    gateway.server.sync().await;
    assert!(get(shared, "/").await.ends_with("winner"));
    gateway.stop().await;
}

#[tokio::test]
async fn dropping_prepared_runtime_releases_ports_without_a_generation() {
    let gateway = Gateway::new("discarded-initial-bind");
    let added = address();
    let candidate = snapshot(&gateway, &config(&[added], "respond \"unpublished\""), 1);
    let outcome = gateway
        .store
        .attempt(&candidate, &gateway.registry, |preparation| {
            let runtime = prepare_candidate(&gateway, preparation)?;
            assert!(gateway.binds.bound().is_empty());
            assert!(std::net::TcpListener::bind(added).is_err());
            drop(runtime);
            assert!(std::net::TcpListener::bind(added).is_ok());
            Err("host cancelled preparation".to_string())
        });
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert!(gateway.store.current().is_none());
    assert!(gateway.binds.bound().is_empty());
    assert!(std::net::TcpListener::bind(added).is_ok());
    gateway.stop().await;
}
