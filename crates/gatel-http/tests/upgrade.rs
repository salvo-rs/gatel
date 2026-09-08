//! Upgraded connections: the handshake, the tunnel, and what must not happen.

mod support;

use std::time::Duration;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn open_tunnel(address: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .await
        .unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(head.starts_with(b"HTTP/1.1 101"));
    stream
}

#[tokio::test]
async fn tunnel_keeps_connection_capacity_and_generation_until_closed() {
    let gateway = Gateway::new("upgrade-lifetime");
    let port = free_port();
    let backend = upgrade_backend().await;
    let source = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}"; limits connections=1; }}
site "*" {{ route "/ws" {{ proxy "{backend}"; }}; route "/" {{ respond "{body}"; }} }}
"#
        )
    };
    let outcome = gateway.apply(&source("old")).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let old = gateway.store.current().unwrap();
    let weak = std::sync::Arc::downgrade(&old);
    let metrics = old.runtime.listeners.values().next().unwrap().metrics.clone();
    drop(old);
    let mut tunnel = open_tunnel(gateway.address()).await;
    gateway.apply(&source("new")).await;
    assert!(weak.upgrade().is_some(), "upgrade released its generation");
    assert_eq!(metrics.connections_active.get(), 1);
    let mut waiting = TcpStream::connect(gateway.address()).await.unwrap();
    waiting
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(30), waiting.read(&mut byte))
            .await
            .is_err(),
        "upgrade stopped counting toward max-connections"
    );
    tunnel.write_all(b"ping").await.unwrap();
    let mut echo = [0; 9];
    tunnel.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"echo:ping");
    drop(tunnel);
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(2), waiting.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.ends_with("new"));
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    gateway.stop().await;
}

#[tokio::test]
async fn shutdown_deadline_cancels_upgraded_io() {
    let gateway = Gateway::new("upgrade-shutdown");
    let port = free_port();
    let backend = upgrade_backend().await;
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}"; grace-period "50ms"; }}
site "*" {{ route "/ws" {{ proxy "{backend}"; }} }}
"#
        ))
        .await;
    let mut tunnel = open_tunnel(gateway.address()).await;
    let metrics = gateway
        .store
        .current()
        .unwrap()
        .runtime
        .listeners
        .values()
        .next()
        .unwrap()
        .metrics
        .clone();
    tokio::time::timeout(Duration::from_secs(2), gateway.server.shutdown())
        .await
        .unwrap();
    assert_eq!(metrics.connections_active.get(), 0);
    let mut bytes = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(1), tunnel.read_to_end(&mut bytes)).await;
    assert!(closed.is_ok(), "upgraded socket survived shutdown");
}

#[tokio::test]
async fn an_upgrade_is_forwarded_and_the_tunnel_carries_bytes() {
    let gateway = Gateway::new("upgrade");
    let port = free_port();
    let backend = upgrade_backend().await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/ws" {{ proxy "{backend}" }}
}}
"#
        ))
        .await;

    let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: example.com\r\n\
              Connection: Upgrade\r\nUpgrade: websocket\r\n\
              Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();

    // Read the response head only; the rest of the socket is the tunnel.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut byte))
        .await
        .is_ok_and(|result| result.is_ok())
    {
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let response = String::from_utf8_lossy(&head).to_string();
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");
    assert!(
        response.to_lowercase().contains("upgrade: websocket"),
        "{response}"
    );

    // The connection is no longer HTTP: whatever is written now comes back
    // through the backend.
    stream.write_all(b"ping").await.unwrap();
    let mut buffer = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("the tunnel should carry bytes")
        .unwrap();
    assert_eq!(&buffer[..read], b"echo:ping");
}

#[tokio::test]
async fn an_upgraded_connection_is_not_returned_to_the_pool() {
    let gateway = Gateway::new("upgrade-pool");
    let port = free_port();
    let backend = upgrade_backend().await;

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/ws\" {{ proxy \"{backend}\" }} }}\n"
        ))
        .await;

    let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .await
        .unwrap();
    let mut buffer = [0u8; 256];
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await;

    let committed = gateway.store.current().unwrap();
    let stats = committed.runtime.upstreams.iter().next().unwrap().pool.stats();
    assert_eq!(stats[0].opened, 1);
    assert_eq!(
        stats[0].idle, 0,
        "a tunnelled connection must never be handed to another request"
    );
}

#[tokio::test]
async fn an_ordinary_request_still_works_on_a_route_that_can_upgrade() {
    let gateway = Gateway::new("upgrade-plain");
    let port = free_port();
    let backend = backend("plain").await;

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ proxy \"{backend}\" }} }}\n"
        ))
        .await;

    // No upgrade headers: the request is proxied normally.
    assert!(get(gateway.address(), "/").await.contains("plain"));
}

#[tokio::test]
async fn an_upgrade_to_an_http2_upstream_is_refused_clearly() {
    let gateway = Gateway::new("upgrade-h2");
    let port = free_port();
    let upstream = backend("h2").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{upstream}"
    protocol "http2"
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nConnection: close\r\n\r\n",
    )
    .await;
    // A gateway error, not a confusing answer from a backend that was sent an
    // upgrade it cannot perform.
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
}
