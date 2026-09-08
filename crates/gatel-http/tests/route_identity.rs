mod support;
use support::*;

#[tokio::test]
async fn conditional_and_fallback_routes_have_independent_rate_limits() {
    let gateway = Gateway::new("route-identity");
    let port = free_port();
    let config = format!(
        r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{
 route "/" {{ match header="x-canary=1"; rate-limit max=1 window="1h"; respond "canary"; }};
 route "/" {{ rate-limit max=1 window="1h"; respond "stable"; }};
}}"#
    );
    assert!(matches!(
        gateway.apply(&config).await,
        gatel_plan::Outcome::Started { .. }
    ));
    let stable = get(gateway.address(), "/").await;
    assert!(stable.starts_with("HTTP/1.1 200"), "{stable}");
    assert!(stable.ends_with("stable"));
    assert!(get(gateway.address(), "/").await.starts_with("HTTP/1.1 429"));
    let canary_request = "GET / HTTP/1.1\r\nHost: test\r\nX-Canary: 1\r\nConnection: close\r\n\r\n";
    let canary = request(gateway.address(), canary_request).await;
    assert!(canary.starts_with("HTTP/1.1 200"), "{canary}");
    assert!(canary.ends_with("canary"));
    assert!(
        request(gateway.address(), canary_request)
            .await
            .starts_with("HTTP/1.1 429")
    );
    // A comment-only reload must not reset either independently owned counter.
    assert!(matches!(
        gateway.apply(&format!("// same intent\n{config}")).await,
        gatel_plan::Outcome::NoOp { .. }
    ));
    assert!(get(gateway.address(), "/").await.starts_with("HTTP/1.1 429"));
    assert!(
        request(gateway.address(), canary_request)
            .await
            .starts_with("HTTP/1.1 429")
    );
    gateway.stop().await;
}
