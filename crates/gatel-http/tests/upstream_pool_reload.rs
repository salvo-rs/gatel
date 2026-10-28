//! Reused H1 pools adopt their current policies only on successful publication.
mod support;

use std::sync::Arc;
use std::time::Duration;

use gatel_plan::{Outcome, Timeouts};
use support::*;

fn snapshot(gateway: &Gateway, text: &str, revision: u64) -> gatel_model::Snapshot {
    let path = gateway.directory.join("gatel.kdl");
    std::fs::write(&path, text).unwrap();
    let mut source = gatel_kdl::snapshot(&path, gatel_model::SourceId::file("main"), None).unwrap();
    source.rev = gatel_model::SourceRev(revision);
    gatel_model::Snapshot::new().with(source)
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
async fn failed_pool_policy_candidate_and_noop_preserve_live_admission_and_idle_policy() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                connections.push(listener.accept().await.unwrap().0);
            }
        });
        let gateway = Gateway::new("upstream-pool-policies");
        let port = free_port();
        let config = |capacity, idle, extra: &str| format!(
            "global {{ http \"127.0.0.1:{port}\"; }}\nupstream \"api\" {{ endpoint \"{backend}\"; pool max-per-endpoint={capacity} max-idle-per-endpoint={idle}; }}\nsite \"*\" {{ route \"/\" {{ proxy upstream=\"api\"; }}; {extra} }}"
        );
        let original = config(4, 4, "");
        assert!(matches!(gateway.apply(&original).await, Outcome::Started { .. }));
        let before = gateway.store.current().unwrap();
        let upstream = before.runtime.upstreams.iter().next().unwrap().clone();
        let pool = upstream.pool.clone();
        let endpoint = &upstream.endpoints.all()[0];
        let timeouts = Timeouts { queue: Duration::from_millis(40), ..Default::default() };
        let mut held = Vec::new();
        for _ in 0..4 { held.push(pool.checkout(endpoint, &timeouts).await.unwrap()); }
        drop(held.pop());
        assert_eq!(pool.stats()[0].idle, 1);
        let failed = config(1, 0, "route \"/bad\" { file-server root=\"missing-root\"; };");
        assert!(matches!(gateway.apply(&failed).await, Outcome::PrepareFailed(_)));
        assert!(Arc::ptr_eq(&before, &gateway.store.current().unwrap()));
        assert_eq!(pool.stats()[0].idle, 1, "failed prepare did not clear old idle policy");
        let fourth = pool.checkout(endpoint, &timeouts).await.unwrap();
        assert_eq!(pool.stats()[0].in_use, 4);
        drop(fourth);
        assert!(matches!(gateway.apply(&original).await, Outcome::NoOp { .. }));
        assert_eq!(pool.stats()[0].idle, 1);

        assert!(matches!(gateway.apply(&config(1, 0, "")).await, Outcome::Reloaded { .. }));
        let current = gateway.store.current().unwrap();
        let after = current.runtime.upstreams.iter().next().unwrap();
        assert!(Arc::ptr_eq(&pool, &after.pool), "compatible publication keeps the pool");
        assert_eq!(pool.stats()[0].idle, 0);
        drop(held.pop());
        drop(held.pop());
        assert!(matches!(pool.checkout(endpoint, &timeouts).await, Err(gatel_state::CheckoutError::QueueTimeout(_))));
        drop(held);
        let only = pool.checkout(endpoint, &timeouts).await.unwrap();
        assert!(matches!(pool.checkout(endpoint, &timeouts).await, Err(gatel_state::CheckoutError::QueueTimeout(_))));
        drop(only);
        assert_eq!(pool.stats()[0].idle, 0);

        let stale = snapshot(&gateway, &config(4, 4, ""), 100);
        let winner = snapshot(&gateway, &config(2, 0, "route \"/winner\" { respond \"ok\"; };"), 101);
        let outcome = gateway.store.attempt(&stale, &gateway.registry, |preparation| {
            let candidate = prepare_candidate(&gateway, preparation)?;
            let outcome = gateway.store.attempt(&winner, &gateway.registry, |preparation| {
                prepare_candidate(&gateway, preparation)
            });
            assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
            Ok(candidate)
        });
        assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
        let first = pool.checkout(endpoint, &timeouts).await.unwrap();
        let second = pool.checkout(endpoint, &timeouts).await.unwrap();
        assert!(matches!(pool.checkout(endpoint, &timeouts).await, Err(gatel_state::CheckoutError::QueueTimeout(_))),
            "stale candidate must not overwrite the winner's capacity");
        drop((first, second));
        assert_eq!(pool.stats()[0].idle, 0, "stale candidate must not restore idle storage");
        gateway.stop().await;
        peer.abort();
        let _ = peer.await;
    }).await.unwrap();
}
