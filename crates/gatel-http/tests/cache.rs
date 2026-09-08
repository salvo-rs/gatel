//! `cache`, end to end.
//!
//! The unit tests in `gatel-bundle-std` cover the storability rules and the
//! store. These cover the two things only a running gateway can show: that a
//! hit really does skip the origin, and that a hit never skips a guard.

mod support;

#[tokio::test]
async fn cache_namespace_changes_when_a_named_upstream_changes_behavior() {
    let gateway = Gateway::new("cache-execution-identity");
    let port = free_port();
    let (old, old_visits) = counting_backend("cache-control: max-age=300\r\nx-origin: old\r\n").await;
    let (new, new_visits) = counting_backend("cache-control: max-age=300\r\nx-origin: new\r\n").await;
    let config = |backend| {
        format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nupstream \"origin\" {{ endpoint \"{backend}\"; }}\nsite \"*\" {{ cache; route \"/\" {{ proxy upstream=\"origin\"; }} }}"
        )
    };
    gateway.apply(&config(old)).await;
    let first = get(gateway.address(), "/").await;
    assert!(first.contains("x-origin: old"), "{first}");
    assert!(get(gateway.address(), "/").await.contains("hit;"));
    gateway.apply(&config(new)).await;
    let changed = get(gateway.address(), "/").await;
    assert!(
        changed.contains("x-origin: new"),
        "old response survived new upstream: {changed}"
    );
    assert!(get(gateway.address(), "/").await.contains("hit;"));
    assert_eq!(old_visits.load(Ordering::SeqCst), 1);
    assert_eq!(new_visits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_budget_reload_is_effective_and_failed_prepare_preserves_active_cache() {
    let gateway = Gateway::new("cache-budget-reload");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    let config = |size: u64, extra: &str| {
        format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ cache max-size={size} max-entry=64; proxy \"{backend}\"; }};\n{extra}\n}}"
        )
    };
    gateway.apply(&config(4096, "")).await;
    let address = gateway.address();
    get(address, "/page").await;
    assert!(get(address, "/page").await.contains("hit;"));
    let unrelated = gateway
        .apply(&config(4096, "route \"/unrelated\" { respond \"new\"; }"))
        .await;
    assert!(
        matches!(unrelated, gatel_plan::Outcome::Reloaded { .. }),
        "{unrelated:?}"
    );
    assert!(
        get(address, "/page").await.contains("hit;"),
        "unchanged policy must survive a real reload, not just NoOp"
    );
    let failed = gateway
        .apply(&config(
            128,
            "route \"/broken\" { file-server root=\"missing-root\"; }",
        ))
        .await;
    assert!(
        matches!(failed, gatel_plan::Outcome::PrepareFailed(_)),
        "{failed:?}"
    );
    assert!(get(address, "/page").await.contains("hit;"));
    assert_eq!(reached.load(Ordering::SeqCst), 1);
    assert!(matches!(
        gateway.apply(&config(128, "")).await,
        gatel_plan::Outcome::Reloaded { .. }
    ));
    for _ in 0..2 {
        let response = get(address, "/page").await;
        assert!(
            !response.contains("hit;"),
            "old budget survived reload: {response}"
        );
        assert!(response.contains("fill-rejected"), "{response}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 3);
    gateway.apply(&config(4096, "")).await;
    assert!(get(address, "/page").await.ends_with("visit 4"));
    assert!(get(address, "/page").await.contains("hit;"));
}

#[tokio::test]
async fn successful_writes_invalidate_same_origin_response_links_only() {
    let gateway = Gateway::new("cache-related-invalidation");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\nlocation: ../read\r\ncontent-location: http://other.example/foreign\r\n").await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ cache; route \"/*\" {{ proxy \"{backend}\"; }} }}")).await;
    get(gateway.address(), "/read").await;
    let foreign = "GET /foreign HTTP/1.1\r\nHost: other.example\r\nConnection: close\r\n\r\n";
    request(gateway.address(), foreign).await;
    request(
        gateway.address(),
        "POST /dir/write HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let updated = get(gateway.address(), "/read").await;
    assert!(updated.ends_with("visit 4"), "{updated}");
    let untouched = request(gateway.address(), foreign).await;
    assert!(
        untouched.ends_with("visit 2") && untouched.contains("hit;"),
        "{untouched}"
    );
    assert_eq!(reached.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_successful_write_invalidates_the_cached_request_uri() {
    let gateway = Gateway::new("cache-write-invalidate");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ cache; route \"/\" {{ proxy \"{backend}\"; }} }}")).await;
    get(gateway.address(), "/").await;
    assert!(get(gateway.address(), "/").await.contains("hit;"));
    let written = request(
        gateway.address(),
        "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(written.starts_with("HTTP/1.1 200"), "{written}");
    let next = get(gateway.address(), "/").await;
    assert!(next.ends_with("visit 3"), "{next}");
    assert_eq!(reached.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cache_variant_uses_every_line_of_a_repeated_request_header() {
    let gateway = Gateway::new("cache-vary-repeated-header");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\nvary: x-variant\r\n").await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
    for (value, visit) in [("one", 1), ("two", 2), ("one", 1)] {
        let response = request(gateway.address(), &format!("GET / HTTP/1.1\r\nHost: example.com\r\nX-Variant: common\r\nX-Variant: {value}\r\nConnection: close\r\n\r\n")).await;
        assert!(response.ends_with(&format!("visit {visit}")), "{response}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn fresh_cache_evaluates_client_validators_without_contacting_origin() {
    let gateway = Gateway::new("cache-conditional");
    let port = free_port();
    let (backend, reached) = counting_backend(
        "cache-control: max-age=300\r\netag: \"a,b\"\r\nlast-modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n",
    )
    .await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
    get(gateway.address(), "/").await;
    for (headers, status) in [
        ("If-None-Match: W/\"a,b\", \"other\"\r\n", 304),
        ("If-None-Match: *\r\n", 304),
        ("If-Modified-Since: Sun, 06 Nov 1994 08:49:37 GMT\r\n", 304),
        (
            "If-None-Match: \"other\"\r\nIf-Modified-Since: Sun, 06 Nov 1994 08:49:37 GMT\r\n",
            200,
        ),
        ("If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n", 200),
    ] {
        let response = request(
            gateway.address(),
            &format!("GET / HTTP/1.1\r\nHost: example.com\r\n{headers}Connection: close\r\n\r\n"),
        )
        .await;
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
        if status == 304 {
            assert!(response.ends_with("\r\n\r\n"), "{response}");
            assert!(response.contains("etag: \"a,b\""), "{response}");
        } else {
            assert!(response.ends_with("visit 1"), "{response}");
        }
    }
    assert_eq!(reached.load(Ordering::SeqCst), 1);
    for headers in [
        "If-Match: \"a,b\"\r\n",
        "Range: bytes=0-1\r\n",
        "If-None-Match: malformed\r\n",
    ] {
        request(
            gateway.address(),
            &format!("GET / HTTP/1.1\r\nHost: example.com\r\n{headers}Connection: close\r\n\r\n"),
        )
        .await;
    }
    assert_eq!(reached.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn cache_only_requests_never_contact_origin_on_miss_or_constraint_failure() {
    let gateway = Gateway::new("cache-only");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ cache; route \"/*\" {{ proxy \"{backend}\"; }} }}")).await;
    for method in ["GET", "HEAD"] {
        let response = request(gateway.address(), &format!("{method} /cold HTTP/1.1\r\nHost: example.com\r\nCache-Control: only-if-cached\r\nConnection: close\r\n\r\n")).await;
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        if method == "HEAD" {
            assert!(response.ends_with("\r\n\r\n"), "{response}");
        }
    }
    assert_eq!(reached.load(Ordering::SeqCst), 0);
    get(gateway.address(), "/warm").await;
    let hit = request(gateway.address(), "GET /warm HTTP/1.1\r\nHost: example.com\r\nCache-Control: only-if-cached\r\nConnection: close\r\n\r\n").await;
    assert!(hit.starts_with("HTTP/1.1 200") && hit.contains("hit;"), "{hit}");
    for extra in ["no-cache", "no-store", "min-fresh=400", "max-age=0"] {
        let response = request(gateway.address(), &format!("GET /warm HTTP/1.1\r\nHost: example.com\r\nCache-Control: only-if-cached\r\nCache-Control: {extra}\r\nConnection: close\r\n\r\n")).await;
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_age_and_request_freshness_are_enforced() {
    for (label, headers, constraint, expected) in [
        (
            "cache-old-date",
            "cache-control: max-age=300\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\n",
            "",
            2,
        ),
        (
            "cache-invalid-age",
            "cache-control: max-age=300\r\nage: -1\r\n",
            "",
            2,
        ),
        (
            "cache-aged-out",
            "cache-control: max-age=60\r\nage: 60\r\n",
            "",
            2,
        ),
        (
            "cache-max-age",
            "cache-control: max-age=300\r\nage: 100\r\n",
            "max-age=50",
            2,
        ),
        (
            "cache-min-fresh",
            "cache-control: max-age=300\r\nage: 100\r\n",
            "min-fresh=250",
            2,
        ),
        (
            "cache-fresh-enough",
            "cache-control: max-age=300\r\nage: 100\r\n",
            "max-age=200, min-fresh=10",
            1,
        ),
    ] {
        let gateway = Gateway::new(label);
        let port = free_port();
        let (backend, reached) = counting_backend(headers).await;
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{label}: {outcome:?}"
        );
        get(gateway.address(), "/").await;
        let response = request(gateway.address(), &format!("GET / HTTP/1.1\r\nHost: example.com\r\nCache-Control: {constraint}\r\nConnection: close\r\n\r\n")).await;
        assert_eq!(reached.load(Ordering::SeqCst), expected, "{label}: {response}");
        assert_eq!(response.contains("hit;"), expected == 1, "{response}");
    }
}

#[tokio::test]
async fn cache_control_restrictions_cannot_hide_on_another_line() {
    for (label, headers) in [
        (
            "cache-control-private",
            "cache-control: max-age=300\r\ncache-control: private=\"x-user, x-role\"\r\n",
        ),
        (
            "cache-control-no-store",
            "cache-control: public, max-age=300\r\ncache-control: no-store\r\n",
        ),
        (
            "cache-control-duplicate",
            "cache-control: max-age=300\r\ncache-control: max-age=0\r\n",
        ),
        (
            "cache-control-invalid",
            "cache-control: max-age=300, s-maxage=invalid\r\n",
        ),
    ] {
        let gateway = Gateway::new(label);
        let port = free_port();
        let (backend, reached) = counting_backend(headers).await;
        gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
        for _ in 0..2 {
            get(gateway.address(), "/").await;
        }
        assert_eq!(reached.load(Ordering::SeqCst), 2, "{label}");
    }
}

#[tokio::test]
async fn request_cache_control_checks_all_lines_and_quoted_zero() {
    let gateway = Gateway::new("cache-control-request");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
    get(gateway.address(), "/").await;
    for policy in ["no-cache", "no-store", "max-age = \"0\""] {
        let response = request(gateway.address(), &format!("GET / HTTP/1.1\r\nHost: example.com\r\nCache-Control: public\r\nCache-Control: {policy}\r\nConnection: close\r\n\r\n")).await;
        assert!(!response.contains("hit;"), "{response}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn dynamic_encoding_respects_representation_metadata() {
    for (label, headers, request_policy, encoded) in [
        (
            "encode-validator",
            "etag: \"original\"\r\ncontent-digest: sha-256=:YWJj:\r\naccept-ranges: bytes\r\n",
            "",
            true,
        ),
        (
            "encode-response-policy",
            "cache-control: public\r\ncache-control: no-transform\r\netag: \"original\"\r\n",
            "",
            false,
        ),
        (
            "encode-request-policy",
            "etag: \"original\"\r\n",
            "Cache-Control: no-transform\r\n",
            false,
        ),
        (
            "encode-range",
            "content-range: bytes 0-6/100\r\netag: \"original\"\r\n",
            "",
            false,
        ),
    ] {
        let gateway = Gateway::new(label);
        let port = free_port();
        let (backend, _) = counting_backend(headers).await;
        gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ encode gzip min-size=0; proxy \"{backend}\"; }} }}")).await;
        let response = request(gateway.address(), &format!("GET / HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\n{request_policy}Connection: close\r\n\r\n")).await;
        let head = response.split("\r\n\r\n").next().unwrap().to_lowercase();
        assert_eq!(
            head.contains("content-encoding: gzip"),
            encoded,
            "{label}: {head}"
        );
        if encoded {
            assert!(head.contains("etag: w/\"original\""), "{head}");
            assert!(!head.contains("content-digest:"), "{head}");
            assert!(!head.contains("accept-ranges:"), "{head}");
        } else {
            assert!(head.contains("etag: \"original\""), "{head}");
            assert!(response.ends_with("visit 1"), "{response}");
        }
    }
}

#[tokio::test]
async fn encoding_preserves_origin_vary_dimensions_and_wildcard() {
    for (label, headers, expected_visits) in [
        (
            "cache-encode-language",
            "cache-control: max-age=300\r\nvary: accept-language\r\n",
            2,
        ),
        (
            "cache-encode-star",
            "cache-control: max-age=300\r\nvary: *\r\n",
            3,
        ),
        (
            "cache-encode-multiline",
            "cache-control: max-age=300\r\nvary: origin\r\nvary: accept-language\r\n",
            2,
        ),
        (
            "cache-encode-multistar",
            "cache-control: max-age=300\r\nvary: origin\r\nvary: *\r\n",
            3,
        ),
    ] {
        let gateway = Gateway::new(label);
        let port = free_port();
        let (backend, reached) = counting_backend(headers).await;
        gateway
            .apply(&format!(
                r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ cache
    route "/*" {{ encode gzip; proxy "{backend}"; }}
}}
"#
            ))
            .await;
        for language in ["en", "fr", "en"] {
            let response = request(gateway.address(), &format!(
                "GET /page HTTP/1.1\r\nHost: example.com\r\nAccept-Language: {language}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
            )).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        }
        assert_eq!(reached.load(Ordering::SeqCst), expected_visits, "{label}");
    }
}

#[tokio::test]
async fn a_site_cache_hit_cannot_bypass_a_route_quota() {
    let gateway = Gateway::new("cache-cross-scope-quota");
    let port = free_port();
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    cache order=-2147483648
    route "/*" {{
        rate-limit max=2 window="1h" order=2147483647
        proxy "{backend}"
    }}
}}
"#
        ))
        .await;
    let address = gateway.address();
    let first = get(address, "/page").await;
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    assert!(
        first.to_lowercase().contains("x-ratelimit-remaining: 1"),
        "{first}"
    );
    let second = get(address, "/page").await;
    assert!(second.starts_with("HTTP/1.1 200"), "{second}");
    assert!(second.contains("hit;"), "{second}");
    assert!(
        second.to_lowercase().contains("x-ratelimit-remaining: 0"),
        "{second}"
    );
    let third = request(address, "GET /page HTTP/1.1\r\nHost: example.com\r\nCache-Control: only-if-cached\r\nConnection: close\r\n\r\n").await;
    assert!(
        third.starts_with("HTTP/1.1 429"),
        "cache bypassed route quota: {third}"
    );
    assert_eq!(reached.load(Ordering::SeqCst), 1);
}

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
    let (backend, reached) = counting_backend("cache-control: max-age=300\r\n").await;
    // Bind the backend before choosing a gateway port: otherwise its ephemeral
    // bind can take the port that free_port has just released.
    let port = free_port();

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ cache
        proxy "{backend}" }} }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );

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

    let outcome = gateway
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

    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
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
