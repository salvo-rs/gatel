//! Abortive close, not upload half-close, must cancel a pending cache fill.
mod support;
use std::time::Duration;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn head(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        bytes.push(stream.read_u8().await.unwrap());
    }
}

#[tokio::test]
async fn abortive_h1_disconnect_releases_a_pending_cold_fill() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let gateway = Gateway::new("cache-h1-reset");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (closed, closure) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            head(&mut first).await;
            first.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 1000\r\nConnection: close\r\n\r\npartial").await.unwrap();
            started.send(()).unwrap();
            assert!(first.read_u8().await.is_err(), "unexpected origin data after reset");
            closed.send(()).unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            head(&mut second).await;
            second.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew").await.unwrap();
        });
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let mut client = TcpStream::connect(gateway.address()).await.unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await.unwrap();
        ready.await.unwrap();
        // SO_LINGER=0 produces an abortive close rather than conflating a FIN
        // with a peer that shut down upload but still wants the response.
        socket2::SockRef::from(&client).set_linger(Some(Duration::ZERO)).unwrap();
        drop(client);
        tokio::time::timeout(Duration::from_secs(3), closure).await.expect("H1 reset did not release origin promptly").unwrap();
        let next = get(gateway.address(), "/").await;
        assert!(next.starts_with("HTTP/1.1 200") && next.ends_with("new"), "{next}");
        origin.await.unwrap();
        let cached = get(gateway.address(), "/").await;
        assert!(cached.contains("hit;") && cached.ends_with("new"), "{cached}");
        gateway.stop().await;
    }).await.expect("H1 cache cancellation timed out");
}
