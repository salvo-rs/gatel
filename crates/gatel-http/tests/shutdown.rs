mod support;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn shutdown_waits_for_a_response_already_in_flight() {
    drain_response(false).await;
}

#[tokio::test]
async fn cancelled_shutdown_retries_drain_and_cannot_restart_listeners() {
    drain_response(true).await;
}

async fn drain_response(cancel_first: bool) {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = upstream.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let backend = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
            .await
            .unwrap();
        wait.await.unwrap();
        stream.write_all(b"done").await.unwrap();
    });
    let gateway = support::Gateway::new("drain-response");
    let port = support::free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}"; grace-period "2s"; }}
site "*" {{ route "/" {{ proxy "{target}"; }} }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let mut client = TcpStream::connect(gateway.address()).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(client.read_u8().await.unwrap());
    }
    if cancel_first {
        use std::future::Future;
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut closing = Box::pin(gateway.server.shutdown());
        assert!(closing.as_mut().poll(&mut cx).is_pending());
        drop(closing);
        let mut retry = Box::pin(gateway.server.shutdown());
        assert!(
            retry.as_mut().poll(&mut cx).is_pending(),
            "retry detached the draining connection"
        );
        drop(retry);
    }
    let server = gateway.server.clone();
    let stopped = tokio::spawn(async move {
        server.shutdown().await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!stopped.is_finished(), "shutdown detached the live response");
    release.send(()).unwrap();
    let mut body = Vec::new();
    client.read_to_end(&mut body).await.unwrap();
    assert_eq!(body, b"done");
    tokio::time::timeout(Duration::from_secs(1), stopped)
        .await
        .unwrap()
        .unwrap();
    backend.await.unwrap();
    gateway.server.sync().await;
    assert!(
        gateway.server.active_addresses().await.is_empty(),
        "closed server restarted listeners"
    );
}

#[tokio::test]
async fn deadline_closes_an_incomplete_request_and_returns_its_slot() {
    let gateway = support::Gateway::new("drain-deadline");
    let port = support::free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}"; grace-period "50ms"; }}
site "*" {{ route "/" {{ respond "ok"; }} }}
"#
        ))
        .await;
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
    let mut client = TcpStream::connect(gateway.address()).await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n")
        .await
        .unwrap();
    // Let the accept loop pin the generation before asking it to stop.
    tokio::time::timeout(Duration::from_secs(1), async {
        while metrics.connections_active.get() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), gateway.server.shutdown())
        .await
        .unwrap();
    let mut remaining = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut remaining)).await;
    assert!(result.is_ok(), "socket survived shutdown");
    // EOF or a reset are both valid for an incomplete request.
    assert_eq!(metrics.connections_active.get(), 0);
}

#[tokio::test]
async fn dropping_server_aborts_owned_connections_without_detaching_accept_loops() {
    let gateway = support::Gateway::new("emergency-drop");
    let port = support::free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}"; grace-period "30s"; }}
site "*" {{ route "/" {{ respond "ok"; }}; }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
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
    let mut client = TcpStream::connect(gateway.address()).await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while metrics.connections_active.get() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(gateway);
    // An emergency drop is not graceful drain: it must not leave a detached
    // accept task waiting out the configured thirty-second grace period.
    tokio::time::timeout(Duration::from_secs(2), async {
        while metrics.connections_active.get() != 0 {
            tokio::task::yield_now().await;
        }
        let _ = client.read_to_end(&mut Vec::new()).await;
    })
    .await
    .unwrap();
}
