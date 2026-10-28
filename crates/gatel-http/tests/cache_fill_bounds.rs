//! Oversized cache misses must start forwarding before the origin finishes.
mod support;

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_head(stream: &mut TcpStream) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < 8192);
        head.push(stream.read_u8().await.unwrap());
    }
}

#[tokio::test]
async fn h1_and_h2_forward_known_and_unknown_oversize_before_origin_completion() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for h2 in [false, true] {
            for declared in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let backend = listener.local_addr().unwrap();
                let (finish, finished) = tokio::sync::oneshot::channel();
                let origin = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    read_head(&mut stream).await;
                    if declared {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: public, max-age=600\r\nContent-Length: 9\r\nConnection: close\r\n\r\nabc").await.unwrap();
                    } else {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nCache-Control: public, max-age=600\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n").await.unwrap();
                    }
                    // No timeout or origin EOF can make collection finish:
                    // only starting the downstream response releases this gate.
                    finished.await.unwrap();
                    stream.write_all(if declared { b"defghi" } else { b"3\r\nghi\r\n0\r\n\r\n" }).await.unwrap();
                });
                let gateway = Gateway::new("cache-fill-bound");
                let port = free_port();
                let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ cache max-size=\"8KiB\" max-entry=\"4\"; proxy \"{backend}\"; }} }}")).await;
                assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
                let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
                let request = http::Request::builder().uri("http://example.com/data").body(Empty::<Bytes>::new()).unwrap();
                let mut response = if h2 {
                    let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await.unwrap();
                    tokio::spawn(connection);
                    tokio::time::timeout(Duration::from_secs(2), client.send_request(request)).await.expect("cache buffered an oversized H2 response").unwrap()
                } else {
                    let (mut client, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
                    tokio::spawn(connection);
                    tokio::time::timeout(Duration::from_secs(2), client.send_request(request)).await.expect("cache buffered an oversized H1 response").unwrap()
                };
                assert_eq!(response.status(), 200);
                assert!(response.headers()["cache-status"].to_str().unwrap().contains("too-large"));
                let first = tokio::time::timeout(Duration::from_secs(2), response.body_mut().frame()).await.unwrap().unwrap().unwrap().into_data().unwrap();
                assert!(first.starts_with(b"abc"));
                finish.send(()).unwrap();
                let mut answer = first.to_vec();
                answer.extend_from_slice(&response.into_body().collect().await.unwrap().to_bytes());
                assert_eq!(answer, b"abcdefghi", "h2={h2}, declared={declared}");
                origin.await.unwrap();
                gateway.stop().await;
            }
        }
    }).await.expect("cache fill streaming regression timed out");
}
