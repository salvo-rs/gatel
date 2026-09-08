//! Independent SCGI wire peer behind multiplexed HTTP/2 streams.
mod support;
use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn upload(socket: &mut TcpStream) {
    let mut digits = Vec::new();
    loop {
        let byte = socket.read_u8().await.unwrap();
        if byte == b':' {
            break;
        }
        assert!(byte.is_ascii_digit() && digits.len() < 8);
        digits.push(byte);
    }
    let length: usize = std::str::from_utf8(&digits).unwrap().parse().unwrap();
    assert!(length < 65536);
    let mut environment = vec![0; length];
    socket.read_exact(&mut environment).await.unwrap();
    assert_eq!(socket.read_u8().await.unwrap(), b',');
    assert!(environment.starts_with(b"CONTENT_LENGTH\x005\x00"));
    let fields = environment.split(|byte| *byte == 0).collect::<Vec<_>>();
    assert!(fields.windows(2).any(|pair| pair == [b"SCGI".as_slice(), b"1"]));
    assert!(
        fields
            .windows(2)
            .any(|pair| pair == [b"SERVER_PROTOCOL".as_slice(), b"HTTP/2.0"])
    );
    let mut body = Vec::new();
    socket.read_to_end(&mut body).await.unwrap();
    assert_eq!(body, b"hello"); // EOF here is upload half-close, not cancellation.
}

fn body() -> impl hyper::body::Body<Data = Bytes, Error = Infallible> + Send + Unpin {
    StreamBody::new(futures_util::stream::iter([
        Ok(hyper::body::Frame::data(Bytes::from_static(b"he"))),
        Ok(hyper::body::Frame::data(Bytes::from_static(b"llo"))),
    ]))
}

#[tokio::test]
async fn scgi_h2_reset_before_response_head_releases_the_only_slot() {
    scenario(false).await;
}

#[tokio::test]
async fn scgi_h2_reset_after_response_head_releases_the_only_slot() {
    scenario(true).await;
}

#[tokio::test]
async fn scgi_h2_reload_preserves_connection_generation_and_failed_prepare() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, hold) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            upload(&mut socket).await;
            socket.write_all(b"Content-Type: text/plain\nContent-Length: 3\n\nold").await.unwrap();
            hold.await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut gateway = Gateway::new("scgi-h2-reload");
        gateway.registry = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle()).bundle(gatel_scgi::bundle()).build().unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let source = |script: &str, version: &str| format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/version\" {{ respond \"{version}\"; }}; route \"/*\" {{ scgi \"{address}\" root=\".\" script=\"{script}\" buffer-unknown=#true; }} }}");
        assert!(matches!(gateway.apply(&source("app.marker", "old-config")).await, gatel_plan::Outcome::Started { .. }));
        let old = gateway.store.current().unwrap();
        let weak = std::sync::Arc::downgrade(&old);
        let generation = old.number;
        drop(old);
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap())).await.unwrap();
        let driver = tokio::spawn(connection);
        let make = |path: &str| http::Request::builder().method("POST").uri(format!("http://localhost{path}")).body(body()).unwrap();
        let mut response = client.send_request(make("/app")).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.body_mut().frame().await.unwrap().unwrap().into_data().unwrap(), "ol");
        assert!(matches!(gateway.apply(&source("missing.marker", "bad-config")).await, gatel_plan::Outcome::PrepareFailed(_)));
        assert_eq!(gateway.store.current().unwrap().number, generation);
        let version = client.send_request(make("/version")).await.unwrap();
        assert_eq!(version.into_body().collect().await.unwrap().to_bytes(), "old-config");
        assert!(matches!(gateway.apply(&source("app.marker", "new-config")).await, gatel_plan::Outcome::Reloaded { .. }));
        assert!(weak.upgrade().is_some());
        let version = client.send_request(make("/version")).await.unwrap();
        assert_eq!(version.into_body().collect().await.unwrap().to_bytes(), "old-config");
        let (mut fresh, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap())).await.unwrap();
        let fresh_driver = tokio::spawn(connection);
        let version = fresh.send_request(make("/version")).await.unwrap();
        assert_eq!(version.into_body().collect().await.unwrap().to_bytes(), "new-config");
        release.send(()).unwrap();
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "d");
        peer.await.unwrap();
        assert!(weak.upgrade().is_some(), "the live H2 connection still owns the old generation");
        drop(client);
        driver.abort();
        let _ = driver.await;
        while weak.upgrade().is_some() { tokio::task::yield_now().await; }
        // Releasing the old connection must not tear down the new one.
        let version = fresh.send_request(make("/version")).await.unwrap();
        assert_eq!(version.into_body().collect().await.unwrap().to_bytes(), "new-config");
        drop(fresh);
        fresh_driver.abort();
        let _ = fresh_driver.await;
        gateway.stop().await;
    }).await.expect("H2 reload lifecycle did not complete");
}

async fn scenario(send_head: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (uploaded, ready) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            upload(&mut first).await;
            if send_head {
                first.write_all(b"Content-Type: text/plain\nContent-Length: 100\n\nhello").await.unwrap();
            }
            uploaded.send(()).unwrap();
            // Never complete the first response, including its headers in the
            // pre-head case. Only cancellation can free concurrency=1 promptly.
            let (mut second, _) = listener.accept().await.unwrap();
            upload(&mut second).await;
            second.write_all(b"Content-Type: text/plain\nContent-Length: 2\n\nok").await.unwrap();
            second.shutdown().await.unwrap();
            // Upload FIN cannot prove socket cancellation. Probe the abandoned
            // output direction after the second request proves slot reuse.
            let block = [b'x'; 16 * 1024];
            for _ in 0..1024 {
                if first.write_all(&block).await.is_err() { return; }
            }
            panic!("cancelled SCGI transport accepted 16 MiB after slot reuse");
        });
        let mut gateway = Gateway::new("scgi-h2-reset");
        gateway.registry = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle()).bundle(gatel_scgi::bundle()).build().unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ scgi \"{address}\" root=\".\" script=\"app.marker\" buffer-unknown=#true concurrency=1; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap())).await.unwrap();
        let driver = tokio::spawn(connection);
        let make = || http::Request::builder().method("POST").uri("http://localhost/").body(body()).unwrap();
        let mut pending = Box::pin(client.send_request(make()));
        let first = if send_head {
            let mut response = pending.as_mut().await.unwrap();
            assert_eq!(response.status(), 200);
            let bytes = response.body_mut().frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(bytes, "hell"); // Final byte is held until valid EOF.
            ready.await.unwrap();
            Some(response)
        } else {
            tokio::select! {
                result = ready => result.unwrap(),
                result = &mut pending => panic!("unexpected response before head: {result:?}"),
            }
            None
        };
        let busy = client.send_request(make()).await.unwrap();
        assert_eq!(busy.status(), 503);
        busy.into_body().collect().await.unwrap();
        drop(first);
        drop(pending);
        // A cancellation frame and the next request can race on the server.
        // Retry only capacity refusals, on this same still-live H2 connection.
        loop {
            let response = client.send_request(make()).await.unwrap();
            let status = response.status();
            let content = response.into_body().collect().await.unwrap().to_bytes();
            if status == 503 { tokio::task::yield_now().await; continue; }
            assert_eq!(status, 200);
            assert_eq!(content, "ok");
            break;
        }
        peer.await.unwrap();
        drop(client);
        driver.abort();
        let _ = driver.await;
        gateway.stop().await;
    }).await.expect("SCGI H2 cancellation must release capacity before backend deadline");
}
