//! Admission and idle limits across listeners, generations and HTTP protocols.
mod support;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn config(port: u16, connections: u32, idle: &str) -> String {
    format!(
        "global {{ http \"127.0.0.1:{port}\"; limits connections={connections} idle=\"{idle}\"; }}\nsite \"*\" {{ route \"/*\" {{ respond \"ok\"; }} }}"
    )
}

async fn head(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        response.push(stream.read_u8().await.unwrap());
        assert!(response.len() < 8192);
    }
    response
}

async fn keepalive(address: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert!(head(&mut stream).await.starts_with(b"HTTP/1.1 200"));
    let mut body = [0; 2];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"ok");
    stream
}

#[tokio::test]
async fn unused_listeners_do_not_reserve_process_capacity() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("admission-listeners");
        let first = free_port();
        let second = free_port();
        let kdl = format!("global {{ http \"a\" \"127.0.0.1:{first}\"; http \"b\" \"127.0.0.1:{second}\"; limits connections=1; }}\nsite \"*\" {{ route \"/*\" {{ respond \"ok\"; }} }}");
        assert!(matches!(gateway.apply(&kdl).await, gatel_plan::Outcome::Started { .. }));
        // Neither listener has traffic initially. Alternating only after the
        // first two successful exchanges also catches the old idle reservation.
        for port in [second, second, first, second, first] {
            let response = tokio::time::timeout(Duration::from_secs(1), get(format!("127.0.0.1:{port}").parse().unwrap(), "/")).await.expect("an idle listener stole the process slot");
            assert!(response.ends_with("ok"));
        }
        gateway.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn shrinking_a_full_budget_applies_after_old_connections_exit() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("admission-shrink");
        let port = free_port();
        gateway.apply(&config(port, 4, "30s")).await;
        let mut old = Vec::new();
        for _ in 0..4 {
            old.push(keepalive(gateway.address()).await);
        }
        assert!(matches!(
            gateway.apply(&config(port, 1, "30s")).await,
            gatel_plan::Outcome::Reloaded { .. }
        ));
        drop(old);
        // Queue until old readers are released; no fixed cleanup sleep.
        let only = keepalive(gateway.address()).await;
        let mut queued = TcpStream::connect(gateway.address()).await.unwrap();
        queued
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(150), queued.read(&mut byte))
                .await
                .is_err(),
            "the old four-slot capacity survived the shrink"
        );
        drop(only);
        assert!(head(&mut queued).await.starts_with(b"HTTP/1.1 200"));
        drop(queued);
        gateway.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn idle_covers_protocol_detection_and_unfinished_http2_headers() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("idle-protocols");
        gateway.apply(&config(free_port(), 1, "100ms")).await;
        let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0";
        let mut unfinished = preface.to_vec();
        // An empty HEADERS fragment without END_HEADERS awaits CONTINUATION;
        // no complete request reaches the service or creates activity.
        unfinished.extend_from_slice(b"\0\0\0\x01\0\0\0\0\x01");
        for bytes in [&b""[..], &b"PRI *"[..], &preface[..], &unfinished[..]] {
            let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
            stream.write_all(bytes).await.unwrap();
            let mut response = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut response))
                .await
                .expect("idle protocol negotiation did not release its slot");
        }
        assert!(get(gateway.address(), "/").await.ends_with("ok"));
        gateway.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn active_http2_response_outlives_idle_then_idle_connection_closes() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("idle-active-h2");
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = origin.local_addr().unwrap();
        let origin = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            head(&mut stream).await;
            // Both waiting for an origin head and delivering its streaming
            // body are request activity, rather than an idle H2 connection.
            tokio::time::sleep(Duration::from_millis(400)).await;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabc").await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            stream.write_all(b"def").await.unwrap();
        });
        let port = free_port();
        let kdl = format!("global {{ http \"127.0.0.1:{port}\"; limits idle=\"100ms\"; }}\nsite \"*\" {{ route \"/slow\" {{ proxy \"{backend}\"; }}; route \"/\" {{ respond \"ok\"; }} }}");
        let outcome = gateway.apply(&kdl).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap())).await.unwrap();
        let mut driver = tokio::spawn(connection);
        let request = |path| http::Request::builder().uri(format!("http://localhost{path}")).body(Empty::<Bytes>::new()).unwrap();
        let response = client.send_request(request("/slow")).await.unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "abcdef");
        let response = client.send_request(request("/")).await.unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "ok");
        // Retaining the client must not retain an otherwise idle connection.
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut driver).await.expect("completed HTTP/2 streams kept the connection alive");
        origin.await.unwrap();
        gateway.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn a_slow_http1_upload_is_request_activity_not_a_partial_head() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("idle-active-h1-upload");
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = origin.local_addr().unwrap();
        let origin = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            head(&mut stream).await;
            let mut body = [0; 6];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"abcdef");
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.unwrap();
        });
        let port = free_port();
        let kdl = format!("global {{ http \"127.0.0.1:{port}\"; limits idle=\"100ms\"; }}\nsite \"*\" {{ route \"/*\" {{ proxy \"{backend}\"; }} }}");
        let outcome = gateway.apply(&kdl).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let mut client = TcpStream::connect(gateway.address()).await.unwrap();
        client.write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        client.write_all(b"def").await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
        origin.await.unwrap();
        gateway.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn resetting_http2_before_response_head_does_not_leak_activity() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gateway = Gateway::new("idle-reset-h2");
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = origin.local_addr().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            head(&mut stream).await;
            started.send(()).unwrap();
            assert!(stream.read_u8().await.is_err(), "reset should close the pending origin exchange");
        });
        let port = free_port();
        let kdl = format!("global {{ http \"127.0.0.1:{port}\"; limits idle=\"100ms\"; }}\nsite \"*\" {{ route \"/slow\" {{ proxy \"{backend}\"; }}; route \"/\" {{ respond \"ok\"; }} }}");
        let outcome = gateway.apply(&kdl).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap())).await.unwrap();
        let mut driver = tokio::spawn(connection);
        let request = |path| http::Request::builder().uri(format!("http://localhost{path}")).body(Empty::<Bytes>::new()).unwrap();
        let mut pending = Box::pin(client.send_request(request("/slow")));
        tokio::select! {
            result = ready => result.unwrap(),
            result = &mut pending => panic!("origin unexpectedly answered: {result:?}"),
        }
        drop(pending);
        let response = client.send_request(request("/")).await.unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "ok");
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut driver).await.expect("cancelled stream leaked request activity");
        origin.await.unwrap();
        gateway.stop().await;
    }).await.unwrap();
}
