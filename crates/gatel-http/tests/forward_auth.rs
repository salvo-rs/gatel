//! `forward-auth` end to end.
//!
//! The point of these is not that the directive formats headers correctly —
//! its own unit tests cover that. It is that a directive reaching a backend
//! goes through the *same* machinery a proxied request does: the same declared
//! upstream, the same connection pool, the same compile-time check on the name
//! it refers to. A capability nobody exercises is a capability nobody knows is
//! broken.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use support::{Gateway, backend, free_port, get, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What an authoriser saw and did, so a test can assert on both.
#[derive(Debug, Default)]
struct Ledger {
    /// TCP connections accepted. Fewer than `asks` means the pool is working.
    accepted: AtomicU64,
    asks: AtomicU64,
    /// Whether any request arrived carrying a body.
    saw_a_body: AtomicU64,
}

/// A service that says yes to a request carrying `cookie: session=good` and
/// sends everybody else to a sign-in page.
async fn authoriser(ledger: Arc<Ledger>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            ledger.accepted.fetch_add(1, Ordering::Relaxed);
            let ledger = Arc::clone(&ledger);
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    ledger.asks.fetch_add(1, Ordering::Relaxed);
                    if head.split_once("\r\n\r\n").map(|(_, body)| !body.is_empty()) == Some(true) {
                        ledger.saw_a_body.fetch_add(1, Ordering::Relaxed);
                    }
                    let approved = head.lines().any(|line| {
                        line.to_ascii_lowercase().starts_with("cookie:") && line.contains("good")
                    });
                    let response = if approved {
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nx-auth-user: alice\r\n\r\n"
                            .to_string()
                    } else {
                        let body = "sign in first";
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: https://id.example/sign-in\r\n\
                             Content-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

/// A backend that reports the identity header it was handed, so a test can see
/// what actually reached the application.
async fn echo_identity() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let users: Vec<String> = head
                        .lines()
                        .filter(|line| line.to_ascii_lowercase().starts_with("x-auth-user:"))
                        .filter_map(|line| line.split(':').nth(1))
                        .map(|value| value.trim().to_string())
                        .collect();
                    let body = format!("user={}", users.join(","));
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

fn config(auth: SocketAddr, app: SocketAddr) -> String {
    let port = free_port();
    format!(
        r#"
global {{ http "127.0.0.1:{port}" }}
upstream "auth" {{ endpoint "{auth}" }}
upstream "app" {{ endpoint "{app}" }}
site "*" {{
    route "/*" {{
        forward-auth upstream="auth" path="/verify" {{
            forward-headers "cookie"
            copy-headers "x-auth-user"
        }}
        proxy upstream="app"
    }}
}}
"#
    )
}

async fn gateway_for(tag: &str, auth: SocketAddr, app: SocketAddr) -> Gateway {
    let gateway = Gateway::new(tag);
    let outcome = gateway.apply(&config(auth, app)).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    gateway
}

#[tokio::test]
async fn an_approved_request_reaches_the_application_carrying_what_the_authoriser_said() {
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    let app = echo_identity().await;
    let gateway = gateway_for("fa-allow", auth, app).await;

    let response = request(
        gateway.address(),
        "GET /orders HTTP/1.1\r\nHost: example.com\r\nCookie: session=good\r\n\
         Connection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("user=alice"), "{response}");
    assert_eq!(ledger.asks.load(Ordering::Relaxed), 1);
    gateway.stop().await;
}

#[tokio::test]
async fn a_refusal_reaches_the_client_whole_and_the_application_never_hears_about_it() {
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    // Nothing is listening here. If the request got through, it would fail
    // loudly rather than quietly passing.
    let app: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let gateway = gateway_for("fa-refuse", auth, app).await;

    let response = get(gateway.address(), "/orders").await;

    assert!(response.starts_with("HTTP/1.1 302"), "{response}");
    assert!(response.contains("https://id.example/sign-in"), "{response}");
    // The refusal's own page, not one we invented for it.
    assert!(response.contains("sign in first"), "{response}");
    gateway.stop().await;
}

#[tokio::test]
async fn a_client_cannot_name_itself() {
    // The whole value of forward-auth is that the application can trust the
    // identity header. It can only do that if the gateway overwrites whatever
    // the client sent under the same name.
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    let app = echo_identity().await;
    let gateway = gateway_for("fa-spoof", auth, app).await;

    let response = request(
        gateway.address(),
        "GET /orders HTTP/1.1\r\nHost: example.com\r\nCookie: session=good\r\n\
         X-Auth-User: root\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.contains("user=alice"), "{response}");
    assert!(!response.contains("root"), "{response}");
    gateway.stop().await;
}

#[tokio::test]
async fn the_authoriser_is_reached_over_a_pooled_connection() {
    // This is the reason the capability exists rather than a private HTTP
    // client inside the directive: three requests, three questions, one
    // connection to the authoriser.
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    let app = echo_identity().await;
    let gateway = gateway_for("fa-pool", auth, app).await;

    for _ in 0..3 {
        let response = request(
            gateway.address(),
            "GET /orders HTTP/1.1\r\nHost: example.com\r\nCookie: session=good\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    assert_eq!(ledger.asks.load(Ordering::Relaxed), 3);
    assert_eq!(
        ledger.accepted.load(Ordering::Relaxed),
        1,
        "the questions should share one pooled connection"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn the_authoriser_is_never_shown_the_body() {
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    let app = backend("app").await;
    let gateway = gateway_for("fa-body", auth, app).await;

    let body = "a=1&b=2";
    let response = request(
        gateway.address(),
        &format!(
            "POST /orders HTTP/1.1\r\nHost: example.com\r\nCookie: session=good\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(
        ledger.saw_a_body.load(Ordering::Relaxed),
        0,
        "a question about a request is not the request"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn an_authoriser_that_cannot_be_reached_refuses_rather_than_letting_it_through() {
    // Failing open here would be an authorisation bypass triggered by an
    // outage, which is the worst possible time for one.
    let app = echo_identity().await;
    let gateway = gateway_for("fa-down", "127.0.0.1:1".parse().unwrap(), app).await;

    let response = get(gateway.address(), "/orders").await;

    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    gateway.stop().await;
}

#[tokio::test]
async fn a_cors_preflight_is_answered_without_asking_the_authoriser() {
    // A browser sends a preflight with no cookies, so asking about it would
    // refuse it, and refusing a preflight breaks the request that follows.
    // Nothing here special-cases that: `cors` is a guard at the default order
    // and `forward-auth` sits after every other guard, so the preflight is
    // already answered by the time the question would be asked. This test
    // exists because that is a property of the ordering, and orderings move.
    let ledger = Arc::new(Ledger::default());
    let auth = authoriser(Arc::clone(&ledger)).await;
    let app = echo_identity().await;
    let port = free_port();
    let gateway = Gateway::new("fa-preflight");
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "auth" {{ endpoint "{auth}" }}
upstream "app" {{ endpoint "{app}" }}
site "*" {{
    route "/*" {{
        proxy upstream="app"
        cors {{ origin "https://app.example" }}
        forward-auth upstream="auth" path="/verify"
    }}
}}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );

    let response = request(
        gateway.address(),
        "OPTIONS /orders HTTP/1.1\r\nHost: example.com\r\nOrigin: https://app.example\r\n\
         Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
    assert_eq!(
        ledger.asks.load(Ordering::Relaxed),
        0,
        "a preflight is a question about a request, not a request"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn an_upstream_that_does_not_exist_is_caught_while_compiling() {
    // Not at the first refusal, when the operator is reading the wrong logs.
    let gateway = Gateway::new("fa-unknown");
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{}" }}
upstream "authoriser" {{ endpoint "127.0.0.1:9" }}
site "*" {{
    route "/*" {{
        forward-auth upstream="authorizer" path="/verify"
        respond "never reached"
    }}
}}
"#,
            free_port()
        ))
        .await;

    let gatel_plan::Outcome::Rejected(diagnostics) = outcome else {
        panic!("a name that is not declared should not compile: {outcome:?}");
    };
    let text = diagnostics.to_string();
    assert!(text.contains("unknown upstream `authorizer`"), "{text}");
    // And it should say what the operator probably meant.
    assert!(text.contains("authoriser"), "{text}");
}
