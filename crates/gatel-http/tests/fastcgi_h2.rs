//! Real HTTP/2 streams driving the non-HTTP FastCGI terminal.
mod support;

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn record(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0; 8];
    stream.read_exact(&mut head).await.unwrap();
    assert_eq!(&head[..1], &[1]);
    assert_eq!(&head[2..4], &[0, 1]);
    let size = u16::from_be_bytes([head[4], head[5]]) as usize;
    let mut data = vec![0; size + head[6] as usize];
    stream.read_exact(&mut data).await.unwrap();
    data.truncate(size);
    (head[1], data)
}

async fn send(stream: &mut TcpStream, kind: u8, data: &[u8]) {
    let length = (data.len() as u16).to_be_bytes();
    stream
        .write_all(&[1, kind, 0, 1, length[0], length[1], 0, 0])
        .await
        .unwrap();
    stream.write_all(data).await.unwrap();
}

async fn upload(stream: &mut TcpStream) {
    assert_eq!(record(stream).await.0, 1);
    let mut params = Vec::new();
    loop {
        let (kind, data) = record(stream).await;
        assert_eq!(kind, 4);
        if data.is_empty() {
            break;
        }
        params.extend(data);
    }
    // Independent short-name/value wire vectors, not a codec self-roundtrip.
    assert!(
        params
            .windows(17)
            .any(|bytes| bytes == b"\x0e\x01CONTENT_LENGTH5")
    );
    let protocol = b"\x0f\x08SERVER_PROTOCOLHTTP/2.0";
    assert!(params.windows(protocol.len()).any(|bytes| bytes == protocol));
    let mut body = Vec::new();
    loop {
        let (kind, data) = record(stream).await;
        assert_eq!(kind, 5);
        if data.is_empty() {
            break;
        }
        body.extend(data);
    }
    assert_eq!(body, b"hello");
}

fn request_body() -> impl hyper::body::Body<Data = Bytes, Error = Infallible> + Send + Unpin {
    // StreamBody has no exact size hint; no Content-Length is supplied.
    StreamBody::new(futures_util::stream::iter([
        Ok(hyper::body::Frame::data(Bytes::from_static(b"he"))),
        Ok(hyper::body::Frame::data(Bytes::from_static(b"llo"))),
    ]))
}

#[tokio::test]
async fn unknown_length_h2_upload_and_stream_reset_release_backend_and_capacity() {
    tokio::time::timeout(Duration::from_secs(10), scenario(true))
        .await
        .expect("H2 cancellation must not wait for the 60-second backend deadline");
}

#[tokio::test]
async fn resetting_h2_before_response_head_cancels_backend() {
    tokio::time::timeout(Duration::from_secs(10), scenario(false))
        .await
        .expect("pre-head cancellation must release capacity promptly");
}

async fn scenario(send_head: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, closure) = tokio::sync::oneshot::channel();
    let (uploaded, ready) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        upload(&mut stream).await;
        if send_head {
            send(
                &mut stream,
                6,
                b"Content-Type: text/plain\nContent-Length: 100\n\nhello",
            )
            .await;
        }
        uploaded.send(()).unwrap();
        // The backend intentionally never finishes. RST_STREAM must drop its socket.
        let result = stream.read_u8().await;
        assert!(result.is_err(), "unexpected bytes after terminated STDIN");
        closed.send(()).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        upload(&mut stream).await;
        send(
            &mut stream,
            6,
            b"Content-Type: text/plain\nContent-Length: 2\n\nok",
        )
        .await;
        send(&mut stream, 3, &[0; 8]).await;
    });
    let mut gateway = Gateway::new("fastcgi-h2");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("index.php"), "<?php").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!(r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ fastcgi "{address}" root="." script="index.php" buffer-unknown=#true concurrency=1; }} }}"#)).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
    let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    let make_request = || {
        http::Request::builder()
            .method("POST")
            .uri("http://localhost/")
            .body(request_body())
            .unwrap()
    };
    let mut pending = Box::pin(client.send_request(make_request()));
    let first = if send_head {
        let mut response = pending.as_mut().await.unwrap();
        assert_eq!(response.status(), 200);
        let data = response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(data, "hell");
        ready.await.unwrap();
        Some(response)
    } else {
        tokio::select! {
            result = ready => result.unwrap(),
            result = &mut pending => panic!("backend sent no response head: {result:?}"),
        }
        None
    };
    // Same H2 connection stays usable while the first response owns the only slot.
    let busy = client.send_request(make_request()).await.unwrap();
    assert_eq!(busy.status(), 503);
    busy.into_body().collect().await.unwrap();
    drop(first);
    drop(pending);
    closure.await.unwrap();
    let next = client.send_request(make_request()).await.unwrap();
    assert_eq!(next.status(), 200);
    assert_eq!(next.into_body().collect().await.unwrap().to_bytes(), "ok");
    peer.await.unwrap();
    drop(client);
    driver.abort();
    let _ = driver.await;
    gateway.stop().await;
}
