//! Concurrent fills share a budget; declining and cancelling must keep H2 usable.
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
async fn concurrent_fill_declines_without_waiting_and_reset_releases_the_budget() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (closed, closure) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            head(&mut first).await;
            first.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1\r\na\r\n").await.unwrap();
            started.send(()).unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            head(&mut second).await;
            second.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nok\r\n0\r\n\r\n").await.unwrap();
            drop(second);
            assert!(first.read_u8().await.is_err(), "cancelled fill must close its origin");
            closed.send(()).unwrap();
            let (mut third, _) = listener.accept().await.unwrap();
            head(&mut third).await;
            third.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nok\r\n0\r\n\r\n").await.unwrap();
        });
        let gateway = Gateway::new("cache-fill-budget");
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ cache max-size=\"512\" max-entry=\"512\"; proxy \"{backend}\"; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await.unwrap();
        let driver = tokio::spawn(connection);
        let request = |path: &str| http::Request::builder().uri(format!("http://example.com/{path}")).body(Empty::<Bytes>::new()).unwrap();
        let mut pending = Box::pin(client.send_request(request("first")));
        tokio::select! {
            ready = ready => ready.unwrap(),
            response = &mut pending => panic!("incomplete fill returned early: {response:?}"),
        }
        let second = client.send_request(request("second")).await.unwrap();
        assert!(second.headers()["cache-status"].to_str().unwrap().contains("fill-budget"));
        assert_eq!(second.into_body().collect().await.unwrap().to_bytes(), "ok");
        drop(pending);
        closure.await.unwrap();
        let third = client.send_request(request("third")).await.unwrap();
        assert!(!third.headers()["cache-status"].to_str().unwrap().contains("fill-budget"), "cancelled collector leaked its reservation");
        assert_eq!(third.into_body().collect().await.unwrap().to_bytes(), "ok");
        origin.await.unwrap();
        drop(client);
        driver.abort();
        gateway.stop().await;
    }).await.expect("cache fill admission regression timed out");
}
