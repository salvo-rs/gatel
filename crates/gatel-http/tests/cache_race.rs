//! Deterministic wire-level cache invalidation races.
mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn head(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn a_get_started_before_a_write_cannot_repopulate_the_cache_after_it() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let gateway = Gateway::new("cache-inflight-invalidate");
        let port = free_port();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut old, _) = listener.accept().await.unwrap();
            assert!(head(&mut old).await.starts_with("GET "));
            started_tx.send(()).unwrap();
            let (mut write, _) = listener.accept().await.unwrap();
            assert!(head(&mut write).await.starts_with("POST "));
            write.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            drop(write);
            release_rx.await.unwrap();
            old.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nCache-Control: max-age=300\r\nConnection: close\r\n\r\nold").await.unwrap();
            drop(old);
            let (mut new, _) = listener.accept().await.unwrap();
            assert!(head(&mut new).await.starts_with("GET "));
            new.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nCache-Control: max-age=300\r\nConnection: close\r\n\r\nnew").await.unwrap();
        });
        gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ cache; route \"/\" {{ proxy \"{backend}\"; }} }}")).await;
        let address = gateway.address();
        let earlier = tokio::spawn(async move { get(address, "/").await });
        started_rx.await.unwrap();
        let written = request(address, "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        assert!(written.starts_with("HTTP/1.1 200"), "{written}");
        release_tx.send(()).unwrap();
        let old = earlier.await.unwrap();
        assert!(old.ends_with("old"), "{old}");
        assert!(old.contains("detail=fill-rejected"), "{old}");
        let new = get(address, "/").await;
        assert!(new.ends_with("new"), "{new}");
        origin.await.unwrap();
        let cached = get(address, "/").await;
        assert!(cached.ends_with("new") && cached.contains("hit;"), "{cached}");
    }).await.expect("cache race test timed out");
}
