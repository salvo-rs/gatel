//! Routing and the middleware chain, over a real socket.

mod support;

use gatel_plan::Outcome;
use support::*;

#[tokio::test]
async fn it_serves_the_route_the_compiler_chose() {
    let gateway = Gateway::new("serve");
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/health" {{
        method "GET"
        respond "ok"
    }}
    route "/*" {{ respond "fallback" }}
}}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");

    let address = gateway.address();
    assert!(get(address, "/health").await.contains("ok"));
    assert!(get(address, "/anything").await.contains("fallback"));
}

#[tokio::test]
async fn an_unmatched_request_gets_gatels_own_not_found() {
    let gateway = Gateway::new("notfound");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "example.com" {{
    route "/only" {{ respond "here" }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/elsewhere").await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("no route matched"));

    let other_host = request(
        gateway.address(),
        "GET /only HTTP/1.1\r\nHost: other.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(other_host.contains("no site is configured"), "{other_host}");
}

#[tokio::test]
async fn middleware_runs_in_phase_order_not_declaration_order() {
    let gateway = Gateway::new("phases");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    headers {{ response {{ set "x-site" "outer" }} }}
    route "/" {{
        respond "body"
        headers {{ response {{ add "x-route" "inner" }} }}
    }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(response.contains("x-site: outer"), "{response}");
    assert!(response.contains("x-route: inner"), "{response}");
    assert!(response.contains("body"));
}

#[tokio::test]
async fn a_get_route_also_answers_head() {
    let gateway = Gateway::new("head");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/" {{
        method "GET"
        respond "body text"
    }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "HEAD / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        !response.contains("body text"),
        "a HEAD response carries no body: {response}"
    );
}
