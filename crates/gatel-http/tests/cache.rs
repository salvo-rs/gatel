//! `cache`, end to end.
//!
//! The unit tests in `gatel-bundle-std` cover the storability rules and the
//! store. These cover the two things only a running gateway can show: that a
//! hit really does skip the origin, and that a hit never skips a guard.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A backend that counts how many requests reached it and answers with
/// whatever headers the test asked for.
async fn counting_backend(headers: &'static str) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reached = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&reached);

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let count = Arc::clone(&count);
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let seen = count.fetch_add(1, Ordering::SeqCst) + 1;
                    // The body names the visit, so a stale copy is visible as
                    // the wrong number rather than only as a wrong count.
                    let body = format!("visit {seen}");
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n{headers}\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(answer.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (address, reached)
}

#[tokio::test]
async fn a_second_request_is_answered_without_reaching_the_origin() {
    let gateway = Gateway::new("cache-hit");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        cache
        proxy "{backend}"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let first = get(address, "/page").await;
    assert!(first.contains("visit 1"), "{first}");
    assert!(
        first
            .to_lowercase()
            .contains("cache-status: gatel; fwd=miss; stored"),
        "the first must say it stored:\n{first}"
    );

    let second = get(address, "/page").await;
    assert!(second.contains("visit 1"), "the stored copy, not a new one");
    assert!(
        second.to_lowercase().contains("cache-status: gatel; hit"),
        "{second}"
    );
    assert!(second.to_lowercase().contains("age:"), "a hit carries its age");
    assert_eq!(
        reached.load(Ordering::SeqCst),
        1,
        "the origin must have been asked exactly once"
    );
}

#[tokio::test]
async fn a_different_path_is_a_different_entry() {
    let gateway = Gateway::new("cache-key");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/one").await;
    get(address, "/two").await;
    // A query string is part of what was asked for, so it is part of the key.
    get(address, "/one?page=2").await;
    assert_eq!(reached.load(Ordering::SeqCst), 3);

    get(address, "/one").await;
    assert_eq!(reached.load(Ordering::SeqCst), 3, "and each is reusable");
}

#[tokio::test]
async fn a_response_that_says_nothing_is_not_stored() {
    // The default, and what nginx does without `proxy_cache_valid`: inventing
    // a lifetime for a response that declined to state one is how a cache
    // serves stale pages nobody asked it to.
    let gateway = Gateway::new("cache-no-freshness");
    let port = free_port();
    let (backend, reached) = counting_backend("").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    let first = get(address, "/page").await;
    assert!(
        first.to_lowercase().contains("detail=no-freshness"),
        "and it says why:\n{first}"
    );
    get(address, "/page").await;
    assert_eq!(reached.load(Ordering::SeqCst), 2, "both reached the origin");
}

#[tokio::test]
async fn told_to_keep_it_anyway_it_keeps_it() {
    let gateway = Gateway::new("cache-default-ttl");
    let port = free_port();
    let (backend, reached) = counting_backend("").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache default-ttl="5m"
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/page").await;
    assert!(get(address, "/page").await.contains("visit 1"));
    assert_eq!(reached.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cached_copy_is_never_a_way_past_a_guard() {
    // The reason `cache` is a request-phase directive. If a hit were served
    // before the guards, storing one response would open the route to
    // everybody — which is the worst thing a cache can do after serving one
    // person's session to another.
    let gateway = Gateway::new("cache-guard");
    let port = free_port();
    let (backend, _) = counting_backend("cache-control: max-age=300\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        cache
        ip-filter deny="0.0.0.0/0"
        proxy "{backend}"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    for _ in 0..3 {
        let response = get(address, "/page").await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "a guard must run on every request, hit or not: {response}"
        );
    }
}

#[tokio::test]
async fn a_response_that_sets_a_cookie_is_never_stored() {
    // The one that would be an incident rather than a bug.
    let gateway = Gateway::new("cache-cookie");
    let port = free_port();
    let (backend, reached) =
        counting_backend("cache-control: public, max-age=300\r\nset-cookie: session=abc\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/page").await;
    let second = get(address, "/page").await;
    assert!(second.contains("visit 2"), "each visit gets its own session");
    assert_eq!(reached.load(Ordering::SeqCst), 2);
    assert!(second.to_lowercase().contains("detail=set-cookie"), "{second}");
}

#[tokio::test]
async fn a_client_can_ask_to_go_to_the_origin() {
    let gateway = Gateway::new("cache-no-cache");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/page").await;
    let refreshed = request(
        address,
        "GET /page HTTP/1.1\r\nHost: example.com\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(refreshed.contains("visit 2"), "{refreshed}");
    assert_eq!(reached.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_post_is_never_answered_from_the_cache() {
    let gateway = Gateway::new("cache-post");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    for _ in 0..2 {
        request(
            address,
            "POST /page HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    }
    assert_eq!(reached.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn what_is_stored_survives_a_reload() {
    // A reload emptying the cache would send every request to the origin at
    // once, which is the one moment a gateway can least afford it.
    let gateway = Gateway::new("cache-reload");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    let config = |comment: &str| {
        format!(
            r#"
// {comment}
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        )
    };

    gateway.apply(&config("first")).await;
    let address = gateway.address();
    get(address, "/page").await;
    assert_eq!(reached.load(Ordering::SeqCst), 1);

    gateway
        .apply(&config("second, which changes nothing that matters"))
        .await;
    assert!(get(address, "/page").await.contains("visit 1"));
    assert_eq!(
        reached.load(Ordering::SeqCst),
        1,
        "the reload must not have emptied the cache"
    );
}

#[tokio::test]
async fn age_accumulates_rather_than_restarting_at_this_hop() {
    // A response from an origin that is itself behind a cache arrives with an
    // `Age`. Reporting only our own resident time tells a browser the response
    // is fresher than it is, and the error grows with every hop.
    let gateway = Gateway::new("cache-age");
    let port = free_port();
    let (backend, _) = counting_backend("cache-control: max-age=300\r\nage: 120\r\n").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    get(address, "/page").await;
    let hit = get(address, "/page").await;

    let age: u64 = hit
        .lines()
        .find(|line| line.to_lowercase().starts_with("age:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("a hit must carry an age:\n{hit}"));
    assert!(
        age >= 120,
        "it was already 120 seconds old when it arrived, so it cannot be {age}"
    );
}
