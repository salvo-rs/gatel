//! `cors`, `force-https`, `trailing-slash`, `timeout` and `body-limit`, on the
//! wire.

mod support;

use support::*;

#[tokio::test]
async fn a_preflight_is_answered_without_reaching_the_route() {
    let gateway = Gateway::new("cors-preflight");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/api/*" id="api" {{
        cors methods="GET,POST,DELETE" headers="content-type" max-age="10m" {{
            origin "https://app.example.com"
        }}
        respond "the route answered"
    }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "OPTIONS /api/things HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example.com\r\n\
         Access-Control-Request-Method: DELETE\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
    assert!(
        response.contains("access-control-allow-origin: https://app.example.com"),
        "{response}"
    );
    assert!(
        response.contains("access-control-allow-methods: GET, POST, DELETE"),
        "{response}"
    );
    assert!(response.contains("access-control-max-age: 600"), "{response}");
    assert!(
        !response.contains("the route answered"),
        "a preflight is a question about the route, not a request to it: {response}"
    );
}

#[tokio::test]
async fn an_origin_that_is_not_allowed_is_still_served() {
    let gateway = Gateway::new("cors-refused");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        cors {{ origin "https://app.example.com" }}
        respond "content"
    }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "GET / HTTP/1.1\r\nHost: h\r\nOrigin: https://evil.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    // The browser is what refuses it. Answering 403 here would break every
    // client that is not a browser while protecting nothing.
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("content"), "{response}");
    assert!(!response.contains("access-control-allow-origin"), "{response}");
    assert!(response.contains("vary: origin"), "{response}");
}

#[tokio::test]
async fn a_plaintext_request_is_redirected_to_https() {
    let gateway = Gateway::new("force-https");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    force-https port=8443
    route "/*" id="any" {{ respond "never reached" }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "GET /a/b?c=d HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 308"), "{response}");
    assert!(
        response.contains("location: https://example.com:8443/a/b?c=d"),
        "{response}"
    );
    assert!(!response.contains("never reached"), "{response}");
}

#[tokio::test]
async fn a_path_is_redirected_to_its_one_spelling() {
    let gateway = Gateway::new("trailing-slash");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        trailing-slash "add"
        respond "served"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let redirected = get(address, "/about").await;
    assert!(redirected.starts_with("HTTP/1.1 308"), "{redirected}");
    assert!(redirected.contains("location: /about/"), "{redirected}");

    // Already canonical, so it is served rather than bounced.
    assert!(get(address, "/about/").await.contains("served"));
    // A filename never gets a slash: `/style.css/` is not a thing.
    assert!(get(address, "/style.css").await.contains("served"));
}

#[tokio::test]
async fn a_route_that_hangs_is_answered_for() {
    let gateway = Gateway::new("timeout");
    let port = free_port();
    // A backend that accepts and then says nothing, which is what a hung
    // service looks like from here.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = silent.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        timeout "300ms"
        proxy "{backend}"
    }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(response.contains("did not answer within"), "{response}");
}

#[tokio::test]
async fn a_route_may_be_stricter_about_bodies_than_the_process() {
    let gateway = Gateway::new("body-limit");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}"
    limits body="1MiB"
}}
site "*" id="all" {{
    route "/small" id="small" {{
        body-limit "16"
        respond "accepted"
    }}
    route "/*" id="any" {{ respond "accepted" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let refused = request(
        address,
        "POST /small HTTP/1.1\r\nHost: h\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 413"), "{refused}");

    // The same body on a route without the directive is within the global
    // limit and is served.
    let allowed = request(
        address,
        "POST /other HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
}

#[tokio::test]
async fn guards_run_in_phase_order_whatever_order_they_are_written_in() {
    let gateway = Gateway::new("guard-order");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        respond "content"
        rewrite prefix="/v2"
        force-https
    }}
}}
"#
        ))
        .await;

    // `force-https` is a guard and `rewrite` is a request-phase directive, so
    // the redirect happens whatever the file says.
    let response = get(gateway.address(), "/thing").await;
    assert!(response.starts_with("HTTP/1.1 308"), "{response}");
    assert!(
        response.contains("location: https://example.com/thing"),
        "the path a guard sees is the one the client sent: {response}"
    );
}
