//! Reverse proxying: balancing, forwarding and connection reuse.

mod support;

use std::sync::Arc;

use support::*;

#[tokio::test]
async fn it_proxies_and_balances_across_backends() {
    let gateway = Gateway::new("proxy");
    let port = free_port();
    let a = backend("alpha").await;
    let b = backend("beta").await;

    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}"
    trusted-proxy "127.0.0.0/8"
}}
upstream "api" {{
    endpoint "{a}" id="a"
    endpoint "{b}" id="b"
    balance "round_robin"
}}
site "*" {{
    route "/api/*" {{ proxy upstream="api" }}
    route "/inline" {{ proxy "{a}" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let answers: Vec<String> = {
        let mut answers = Vec::new();
        for _ in 0..4 {
            answers.push(get(address, "/api/x").await);
        }
        answers
    };
    assert_eq!(
        answers.iter().filter(|body| body.contains("alpha")).count(),
        2,
        "{answers:?}"
    );
    assert_eq!(answers.iter().filter(|body| body.contains("beta")).count(), 2);

    assert!(
        get(address, "/inline").await.contains("alpha"),
        "the short form must proxy too"
    );
}

#[tokio::test]
async fn the_client_address_reaches_the_backend() {
    let gateway = Gateway::new("forwarded");
    let port = free_port();
    let upstream = backend("only").await;

    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}"
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{ route "/*" {{ proxy "{upstream}" }} }}
"#
        ))
        .await;

    let direct = get(gateway.address(), "/").await;
    assert!(direct.contains("xff=127.0.0.1"), "{direct}");

    // The loopback peer is trusted here, so its header is believed.
    let forwarded = request(
        gateway.address(),
        "GET / HTTP/1.1\r\nHost: example.com\r\nX-Forwarded-For: 203.0.113.9\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(forwarded.contains("xff=203.0.113.9"), "{forwarded}");
}

#[tokio::test]
async fn an_unreachable_backend_produces_a_gateway_error_not_a_hang() {
    let gateway = Gateway::new("unreachable");
    let port = free_port();
    // A port nothing is listening on.
    let dead = format!("127.0.0.1:{}", free_port());

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "dead" {{
    endpoint "{dead}"
    timeouts connect="300ms" queue="200ms" response-head="1s" overall="5s"
}}
site "*" {{ route "/*" {{ proxy upstream="dead" }} }}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    // The backend address must not appear in what the client is told.
    assert!(!response.contains(&dead), "{response}");
}

#[tokio::test]
async fn connections_are_reused_across_requests() {
    let gateway = Gateway::new("reuse");
    let port = free_port();
    let upstream = backend("pooled").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ proxy "{upstream}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    for _ in 0..3 {
        assert!(get(address, "/").await.contains("pooled"));
    }

    let committed = gateway.store.current().unwrap();
    let stats = committed.runtime.upstreams.iter().next().unwrap().pool.stats();
    assert_eq!(
        stats[0].opened, 1,
        "three requests should have shared one upstream connection"
    );
    assert_eq!(stats[0].reused, 2);
}

#[tokio::test]
async fn a_compatible_reload_keeps_upstream_connections_open() {
    let gateway = Gateway::new("keep-pool");
    let port = free_port();
    let upstream = backend("kept").await;
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{ endpoint "{upstream}" }}
site "*" {{
    route "/" {{ respond "{body}" }}
    route "/api" {{ proxy upstream="api" }}
}}
"#
        )
    };

    gateway.apply(&config("before")).await;
    let address = gateway.address();
    get(address, "/api").await;
    let pool_before = Arc::clone(
        &gateway
            .store
            .current()
            .unwrap()
            .runtime
            .upstreams
            .iter()
            .next()
            .unwrap()
            .pool,
    );

    // Only an unrelated route changed, so the upstream's state must carry over.
    gateway.apply(&config("after")).await;
    let pool_after = Arc::clone(
        &gateway
            .store
            .current()
            .unwrap()
            .runtime
            .upstreams
            .iter()
            .next()
            .unwrap()
            .pool,
    );
    assert!(
        Arc::ptr_eq(&pool_before, &pool_after),
        "an unrelated edit must not drop connections"
    );

    get(address, "/api").await;
    assert_eq!(
        pool_after.stats()[0].opened,
        1,
        "the connection survived the reload"
    );
}
