//! Independent origin checks actual conditional requests and reconstructed bodies.
mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn fresh_cache_does_not_decide_conditions_from_ambiguous_origin_etags() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for (label, tags) in [
            ("duplicate-tag", "ETag: \"v1\"\r\nETag: \"v2\"\r\n"),
            ("duplicate-same-tag", "ETag: \"v1\"\r\nETag: \"v1\"\r\n"),
            ("malformed-tag", "ETag: malformed\r\n"),
        ] {
            let gateway = Gateway::new(label);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = listener.local_addr().unwrap();
            let origin = tokio::spawn(async move {
                let (mut first, _) = listener.accept().await.unwrap();
                head(&mut first).await;
                first.write_all(format!("HTTP/1.1 200 OK\r\n{tags}Cache-Control: max-age=600\r\nContent-Length: 4\r\nConnection: close\r\n\r\nseed").as_bytes()).await.unwrap();
                drop(first);
                let (mut second, _) = listener.accept().await.unwrap();
                assert!(head(&mut second).await.contains("if-none-match: \"v1\""));
                second.write_all(b"HTTP/1.1 200 OK\r\nETag: \"v3\"\r\nCache-Control: max-age=600\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew").await.unwrap();
            });
            let port = free_port();
            let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
            assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
            assert!(get(gateway.address(), "/").await.ends_with("seed"));
            // Prove this is a fresh cached entry, not a bypassed response.
            let cached = get(gateway.address(), "/").await;
            assert!(cached.contains("hit;") && cached.ends_with("seed"), "{cached}");
            let response = request(gateway.address(), "GET / HTTP/1.1\r\nHost: example.com\r\nIf-None-Match: \"v1\"\r\nConnection: close\r\n\r\n").await;
            assert!(response.starts_with("HTTP/1.1 200") && response.ends_with("new"), "{label}: {response}");
            origin.await.unwrap();
            // The origin is now closed: subsequent responses must come from
            // the replacement, not revive the old ambiguous cached entry.
            let cached = get(gateway.address(), "/").await;
            assert!(cached.contains("hit;") && cached.ends_with("new"), "{cached}");
            let not_modified = request(gateway.address(), "GET / HTTP/1.1\r\nHost: example.com\r\nIf-None-Match: \"v3\"\r\nConnection: close\r\n\r\n").await;
            assert!(not_modified.starts_with("HTTP/1.1 304") && not_modified.contains("hit;") && not_modified.ends_with("\r\n\r\n"), "{not_modified}");
        }
    }).await.expect("ambiguous cache validator test timed out");
}

#[tokio::test]
async fn stale_date_validation_reconstructs_or_rejects_origin_metadata() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for (label, metadata, success) in [
            ("date-same", "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n", true),
            ("date-absent", "", true),
            ("date-obsolete", "Last-Modified: Wednesday, 21-Oct-15 07:28:00 GMT\r\n", true),
            ("date-newer", "Last-Modified: Wed, 21 Oct 2015 07:29:00 GMT\r\n", false),
            ("date-invalid", "Last-Modified: invalid\r\n", false),
            ("date-duplicate", "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n", false),
            ("date-new-tag", "ETag: \"upgraded\"\r\n", true),
            ("date-bad-tag", "ETag: invalid\r\n", false),
        ] {
            let gateway = Gateway::new(label);
            let port = free_port();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = listener.local_addr().unwrap();
            let origin = tokio::spawn(async move {
                let (mut first, _) = listener.accept().await.unwrap();
                let first_head = head(&mut first).await;
                assert!(!first_head.contains("if-modified-since:") && !first_head.contains("if-none-match:"));
                first.write_all(b"HTTP/1.1 200 OK\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nAge: 300\r\nCache-Control: max-age=300\r\nContent-Length: 4\r\nConnection: close\r\n\r\nseed").await.unwrap();
                drop(first);
                let (mut second, _) = listener.accept().await.unwrap();
                let second_head = head(&mut second).await;
                assert!(second_head.contains("if-modified-since: wed, 21 oct 2015 07:28:00 gmt"), "{second_head}");
                assert!(!second_head.contains("if-none-match:"));
                second.write_all(format!("HTTP/1.1 304 Not Modified\r\n{metadata}Cache-Control: max-age=600\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                drop(second);
                if !success {
                    // Keep the origin available: eviction must cause an actual
                    // unconditional request, not depend on OS connection-refusal
                    // timing after closing a listening socket.
                    let (mut retry, _) = listener.accept().await.unwrap();
                    let retry_head = head(&mut retry).await;
                    assert!(!retry_head.contains("if-modified-since:") && !retry_head.contains("if-none-match:"), "{label}: {retry_head}");
                    retry.write_all(b"HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-store\r\nContent-Length: 11\r\nConnection: close\r\n\r\norigin-down").await.unwrap();
                }
            });
            gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
            assert!(get(gateway.address(), "/").await.ends_with("seed"));
            let refreshed = get(gateway.address(), "/").await;
            if success {
                origin.await.unwrap();
                assert!(refreshed.starts_with("HTTP/1.1 200") && refreshed.ends_with("seed") && refreshed.contains("fwd-status=304"), "{refreshed}");
                let cached = get(gateway.address(), "/").await;
                assert!(cached.contains("hit;") && cached.ends_with("seed"), "{cached}");
            } else {
                assert!(refreshed.starts_with("HTTP/1.1 502") && refreshed.contains("invalid-validation"), "{refreshed}");
                // Neither the rejected validation nor the expired candidate
                // may become a subsequent cache hit or supply a validator.
                let retry = get(gateway.address(), "/").await;
                assert!(retry.starts_with("HTTP/1.1 503") && retry.ends_with("origin-down") && !retry.contains("hit;"), "{label}: {retry}");
                origin.await.unwrap();
            }
            gateway.stop().await;
        }
    }).await.expect("date revalidation timed out");
}

async fn head(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap().to_lowercase()
}

#[tokio::test]
async fn stale_etag_validation_handles_304_replacement_and_invalid_validator() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for (label, answer, status, body) in [
            ("cache-304", "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nCache-Control: max-age=600\r\nX-Validated: yes\r\nConnection: close\r\n\r\n", 200, "seed"),
            ("cache-200", "HTTP/1.1 200 OK\r\nETag: \"v2\"\r\nCache-Control: max-age=600\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew", 200, "new"),
            ("cache-bad304", "HTTP/1.1 304 Not Modified\r\nETag: \"different\"\r\nConnection: close\r\n\r\n", 502, "502 invalid cache validation response\n"),
        ] {
            let gateway = Gateway::new(label);
            let port = free_port();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = listener.local_addr().unwrap();
            let origin = tokio::spawn(async move {
                let (mut first, _) = listener.accept().await.unwrap();
                assert!(!head(&mut first).await.contains("if-none-match:"));
                first.write_all(b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nAge: 300\r\nCache-Control: max-age=300\r\nContent-Length: 4\r\nConnection: close\r\n\r\nseed").await.unwrap();
                drop(first);
                let (mut second, _) = listener.accept().await.unwrap();
                assert!(head(&mut second).await.contains("if-none-match: \"v1\""));
                second.write_all(answer.as_bytes()).await.unwrap();
            });
            gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
            assert!(get(gateway.address(), "/").await.ends_with("seed"));
            let refreshed = get(gateway.address(), "/").await;
            assert!(refreshed.starts_with(&format!("HTTP/1.1 {status}")) && refreshed.ends_with(body), "{refreshed}");
            origin.await.unwrap();
            if status == 200 {
                let cached = get(gateway.address(), "/").await;
                assert!(cached.contains("hit;") && cached.ends_with(body), "{cached}");
                if label == "cache-304" { assert!(cached.contains("x-validated: yes"), "{cached}"); }
            }
        }
    }).await.expect("revalidation timed out");
}
