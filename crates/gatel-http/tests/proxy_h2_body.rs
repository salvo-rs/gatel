//! Unknown-length HTTP/2 uploads must be forwarded as streams, not empty bodies.
mod support;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn h2_uploads_preserve_unknown_length_fragmented_declared_and_empty_bodies() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        |request: hyper::Request<hyper::body::Incoming>| async move {
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            Ok::<_, Infallible>(hyper::Response::new(Full::new(bytes)))
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let gateway = Gateway::new("proxy-h2-bodies");
        let port = free_port();
        let outcome = gateway.apply(&format!(
            "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ proxy \"{origin}\"; }} }}"
        )).await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{outcome:?}"
        );
        let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
        let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        for method in ["POST", "PUT", "PATCH"] {
            for (declared, empty) in [(false, false), (true, false), (false, true)] {
                let fragments = if empty { vec![] } else { vec!["he", "l", "lo"] };
                let frames = fragments.into_iter().map(|fragment| {
                    Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from_static(fragment.as_bytes())))
                });
                // StreamBody deliberately has no exact size hint. The upstream
                // HTTP/1 connection must receive chunked bytes when CL is absent.
                let mut request = http::Request::builder()
                    .method(method)
                    .uri("http://example.com/upload");
                if declared {
                    request = request.header("content-length", "5");
                }
                let response = client
                    .send_request(
                        request
                            .body(StreamBody::new(futures_util::stream::iter(frames)))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    if empty { "" } else { "hello" },
                    "{method}, declared={declared}, empty={empty}"
                );
            }
        }
        drop(client);
        driver.abort();
        backend.abort();
        gateway.stop().await;
    })
    .await
    .expect("HTTP/2 body regression timed out");
}

#[tokio::test]
async fn h2_unknown_length_upload_is_not_replayed_after_the_origin_received_it() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for declared in [false, true] {
            let bad = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bad_address = bad.local_addr().unwrap();
            let good = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let good_address = good.local_addr().unwrap();
            let seen = Arc::new(AtomicUsize::new(0));
            let bad_peer = tokio::spawn(async move {
                let (stream, _) = bad.accept().await.unwrap();
                let service =
                    hyper::service::service_fn(|request: hyper::Request<hyper::body::Incoming>| async move {
                        assert_eq!(request.into_body().collect().await.unwrap().to_bytes(), "hello");
                        Err::<hyper::Response<Full<Bytes>>, _>(std::io::Error::other(
                            "close after consuming body",
                        ))
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
            let good_seen = seen.clone();
            let good_peer = tokio::spawn(async move {
                let (stream, _) = good.accept().await.unwrap();
                let service =
                    hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        good_seen.fetch_add(1, Ordering::Relaxed);
                        async move {
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            Ok::<_, Infallible>(hyper::Response::new(Full::new(bytes)))
                        }
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
            let gateway = Gateway::new("h2-retry-body");
            let port = free_port();
            let outcome = gateway
                .apply(&format!(
                    r#"global {{ http "127.0.0.1:{port}"; }}
upstream "api" {{
 endpoint "{bad_address}" id="bad" priority=0;
 endpoint "{good_address}" id="good" priority=1;
 balance "first";
 retry mode="replay" attempts=2 buffer="1KiB";
}}
site "*" {{ route "/*" {{ proxy upstream="api"; }} }}"#
                ))
                .await;
            assert!(
                matches!(outcome, gatel_plan::Outcome::Started { .. }),
                "{outcome:?}"
            );
            let io = TokioIo::new(TcpStream::connect(gateway.address()).await.unwrap());
            let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
                .await
                .unwrap();
            let driver = tokio::spawn(connection);
            let frames = futures_util::stream::iter([Ok::<_, Infallible>(hyper::body::Frame::data(
                Bytes::from_static(b"hello"),
            ))]);
            let mut request = http::Request::builder().method("POST").uri("http://example.com/");
            if declared {
                request = request.header("content-length", "5");
            }
            let response = client
                .send_request(request.body(StreamBody::new(frames)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), if declared { 200 } else { 502 });
            response.into_body().collect().await.unwrap();
            assert_eq!(
                seen.load(Ordering::Relaxed),
                usize::from(declared),
                "only a buffered, known-length body may replay"
            );
            bad_peer.await.unwrap();
            good_peer.abort();
            drop(client);
            driver.abort();
            gateway.stop().await;
        }
    })
    .await
    .expect("HTTP/2 retry regression timed out");
}
