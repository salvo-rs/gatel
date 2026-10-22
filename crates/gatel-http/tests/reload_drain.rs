//! Opt-in retirement over real H1/H2 connections and upgraded tunnels.
mod support;

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn config(port: u16, policy: &str, body: &str) -> String {
    format!(
        "global {{ http \"127.0.0.1:{port}\"; {policy} }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\" }} }}\n"
    )
}

fn request() -> http::Request<Empty<Bytes>> {
    http::Request::builder()
        .uri("http://localhost/")
        .body(Empty::new())
        .unwrap()
}

#[tokio::test]
async fn default_keeps_old_connection_but_enabling_drain_retires_it() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("reload-drain-h1");
        let port = free_port();
        gateway.apply(&config(port, "", "old")).await;
        let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(
            TcpStream::connect(gateway.address()).await.unwrap(),
        ))
        .await
        .unwrap();
        let driver = tokio::spawn(connection);
        let first = client.send_request(request()).await.unwrap();
        assert_eq!(first.into_body().collect().await.unwrap().to_bytes(), "old");
        gateway.apply(&config(port, "", "new")).await;
        let pinned = client.send_request(request()).await.unwrap();
        assert_eq!(pinned.into_body().collect().await.unwrap().to_bytes(), "old");
        gateway
            .apply(&config(port, "reload-drain \"1s\"", "latest"))
            .await;
        driver.await.unwrap().unwrap();
        assert!(get(gateway.address(), "/").await.ends_with("latest"));
        gateway.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn h2_retires_idle_connection_and_new_connections_use_the_new_generation() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("reload-drain-h2");
        let port = free_port();
        gateway.apply(&config(port, "reload-drain \"1s\"", "old")).await;
        let (mut client, connection) = hyper::client::conn::http2::handshake(
            TokioExecutor::new(),
            TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap()),
        )
        .await
        .unwrap();
        let driver = tokio::spawn(connection);
        let first = client.send_request(request()).await.unwrap();
        assert_eq!(first.into_body().collect().await.unwrap().to_bytes(), "old");
        // NoOp and rejected candidates must not send GOAWAY.
        assert!(matches!(
            gateway.apply(&config(port, "reload-drain \"1s\"", "old")).await,
            gatel_plan::Outcome::NoOp { .. }
        ));
        assert!(matches!(
            gateway.apply("not-a-valid-configuration").await,
            gatel_plan::Outcome::Rejected(_)
        ));
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let failed = config(port, "reload-drain \"1s\"", "unpublished").replace(
            "reload-drain",
            &format!(
                "http \"busy\" \"{}\"; reload-drain",
                occupied.local_addr().unwrap()
            ),
        );
        assert!(matches!(
            gateway.apply(&failed).await,
            gatel_plan::Outcome::PrepareFailed(_)
        ));
        let still_old = client.send_request(request()).await.unwrap();
        assert_eq!(still_old.into_body().collect().await.unwrap().to_bytes(), "old");
        gateway.apply(&config(port, "reload-drain \"1s\"", "new")).await;
        driver.await.unwrap().unwrap();
        assert!(get(gateway.address(), "/").await.ends_with("new"));
        gateway.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn retirement_bounds_a_connection_still_waiting_for_a_proxy_header() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("reload-drain-handshake");
        let port = free_port();
        gateway.apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" proxy-protocol=#true; trusted-proxy \"127.0.0.0/8\"; limits connections=1 }}\nsite \"*\" {{ route \"/\" {{ respond \"old\" }} }}\n"
        )).await;
        let metrics = gateway.store.current().unwrap().runtime.listeners.values().next().unwrap().metrics.clone();
        let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
        while metrics.connections_active.get() == 0 { tokio::task::yield_now().await; }
        gateway.apply(&config(port, "reload-drain \"20ms\"; limits connections=1", "new")).await;
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        assert!(get(gateway.address(), "/").await.ends_with("new"));
        gateway.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn an_inflight_response_finishes_during_the_grace_period() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("reload-drain-body");
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut stream, _) = backend.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") { head.push(stream.read_u8().await.unwrap()); }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nold").await.unwrap();
            released.await.unwrap();
            stream.write_all(b"end").await.unwrap();
        });
        let port = free_port();
        gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ proxy \"{address}\" }} }}\n")).await;
        let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(
            TcpStream::connect(gateway.address()).await.unwrap(),
        )).await.unwrap();
        let driver = tokio::spawn(connection);
        let response = client.send_request(request()).await.unwrap();
        gateway.apply(&config(port, "reload-drain \"2s\"", "new")).await;
        release.send(()).unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "oldend");
        driver.await.unwrap().unwrap();
        origin.await.unwrap();
        gateway.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn a_tunnel_deadline_survives_another_reload_disabling_the_policy() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("reload-drain-tunnel");
        let backend = upgrade_backend().await;
        let port = free_port();
        gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; limits connections=1 }}\nsite \"*\" {{ route \"/\" {{ proxy \"{backend}\" }} }}\n")).await;
        let old = std::sync::Arc::downgrade(&gateway.store.current().unwrap());
        let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") { head.push(stream.read_u8().await.unwrap()); }
        assert!(head.starts_with(b"HTTP/1.1 101"));
        stream.write_all(b"ping").await.unwrap();
        let mut echo = [0; 9];
        stream.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"echo:ping");
        gateway.apply(&config(port, "reload-drain \"100ms\"; limits connections=1", "new")).await;
        gateway.apply(&config(port, "limits connections=1", "latest")).await;
        assert_eq!(stream.read(&mut echo).await.unwrap(), 0);
        // The old generation and its process connection permit are released.
        assert!(get(gateway.address(), "/").await.ends_with("latest"));
        assert!(old.upgrade().is_none());
        gateway.stop().await;
    }).await.unwrap();
}
