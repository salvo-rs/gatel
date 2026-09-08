//! A newly observed 200 must not leave its superseded copy available.
mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn truncated_new_body_cannot_restore_the_old_copy_or_cache_its_prefix() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for (label, framing, partial) in [
            ("short-length", "Content-Length: 12\r\n", "partial"),
            ("short-chunk", "Transfer-Encoding: chunked\r\n", "c\r\npartial"),
        ] {
            let gateway = Gateway::new(label);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = listener.local_addr().unwrap();
            let origin = tokio::spawn(async move {
                for response in [
                    "HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 4\r\nConnection: close\r\n\r\nseed".to_owned(),
                    format!("HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\n{framing}Connection: close\r\n\r\n{partial}"),
                    "HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 5\r\nConnection: close\r\n\r\nthird".to_owned(),
                ] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    while !bytes.ends_with(b"\r\n\r\n") { bytes.push(stream.read_u8().await.unwrap()); }
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });
            let port = free_port();
            let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
            assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
            assert!(get(gateway.address(), "/").await.ends_with("seed"));
            assert!(get(gateway.address(), "/").await.contains("hit;"));
            // Unlike the generic helper, require EOF or a connection error,
            // never treat an observation timeout as successful completion.
            let mut client = tokio::net::TcpStream::connect(gateway.address()).await.unwrap();
            client.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").await.unwrap();
            let mut broken = Vec::new();
            let _io_result = tokio::time::timeout(std::time::Duration::from_secs(3), client.read_to_end(&mut broken)).await.expect("truncated response did not close");
            assert!(!String::from_utf8_lossy(&broken).contains("seed"));
            let next = get(gateway.address(), "/").await;
            assert!(!next.contains("hit;") && next.ends_with("third"), "{label}: {next}");
            origin.await.unwrap();
            let cached = get(gateway.address(), "/").await;
            assert!(cached.contains("hit;") && cached.ends_with("third"), "{cached}");
        }
    }).await.expect("truncated replacement test timed out");
}

#[tokio::test]
async fn unstoreable_new_responses_retire_the_observed_fresh_copy() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for (label, fields, body) in [
            ("private", "Cache-Control: private\r\n", "private"),
            ("no-store", "Cache-Control: no-store\r\n", "new"),
            ("no-freshness", "", "new"),
            ("invalid-age", "Age: invalid\r\nCache-Control: max-age=600\r\n", "new"),
            ("large", "Cache-Control: max-age=600\r\n", "this response is larger than sixteen bytes"),
        ] {
            let gateway = Gateway::new(label);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = listener.local_addr().unwrap();
            let origin = tokio::spawn(async move {
                for response in [
                    "HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 4\r\nConnection: close\r\n\r\nseed".to_owned(),
                    format!("HTTP/1.1 200 OK\r\n{fields}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()),
                    "HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 5\r\nConnection: close\r\n\r\nthird".to_owned(),
                ] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(stream.read_u8().await.unwrap());
                    }
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let port = free_port();
            let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache max-entry=16; proxy \"{backend}\"; }} }}")).await;
            assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
            assert!(get(gateway.address(), "/").await.ends_with("seed"));
            let cached = get(gateway.address(), "/").await;
            assert!(cached.contains("hit;") && cached.ends_with("seed"), "{cached}");
            let refreshed = request(gateway.address(), "GET / HTTP/1.1\r\nHost: example.com\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").await;
            assert!(refreshed.starts_with("HTTP/1.1 200") && refreshed.ends_with(body), "{refreshed}");
            let next = get(gateway.address(), "/").await;
            assert!(!next.contains("hit;") && next.ends_with("third"), "{label}: {next}");
            origin.await.unwrap();
        }
    }).await.expect("uncacheable replacement timed out");
}
