//! H2 cancellation must reach a cache miss's pending upstream response.
mod support;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
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
async fn reset_before_cached_response_head_releases_origin_and_keeps_h2_usable() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let gateway = Gateway::new("cache-h2-reset");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (closed, closure) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            head(&mut first).await;
            first.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 1000\r\nConnection: close\r\n\r\npartial").await.unwrap();
            started.send(()).unwrap();
            // No server-side EOF/timeout is injected: only downstream reset
            // can release this indefinitely incomplete exchange promptly.
            let result = first.read_u8().await;
            assert!(result.is_err(), "unexpected data on cancelled origin connection");
            closed.send(()).unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            head(&mut second).await;
            second.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew").await.unwrap();
        });
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ cache; proxy \"{backend}\"; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await.unwrap();
        let driver = tokio::spawn(connection);
        let make_request = || http::Request::builder().uri("http://example.com/").body(Empty::<Bytes>::new()).unwrap();
        let mut pending = Box::pin(client.send_request(make_request()));
        tokio::select! {
            result = ready => result.unwrap(),
            result = &mut pending => panic!("incomplete cache fill returned early: {result:?}"),
        }
        drop(pending);
        tokio::time::timeout(Duration::from_secs(3), closure).await.expect("reset did not close origin promptly").unwrap();
        let next = client.send_request(make_request()).await.unwrap();
        assert_eq!(next.status(), 200);
        assert_eq!(next.into_body().collect().await.unwrap().to_bytes(), "new");
        origin.await.unwrap();
        // No origin is listening now; this must be the complete replacement.
        let cached = client.send_request(make_request()).await.unwrap();
        assert_eq!(cached.status(), 200);
        assert!(cached.headers()["cache-status"].to_str().unwrap().contains("hit;"));
        assert_eq!(cached.into_body().collect().await.unwrap().to_bytes(), "new");
        drop(client);
        driver.abort();
        let _ = driver.await;
        gateway.stop().await;
    }).await.expect("H2 cache cancellation timed out");
}
