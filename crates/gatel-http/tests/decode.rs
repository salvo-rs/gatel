mod support;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn encode(plain: &[u8], coding: &str) -> Vec<u8> {
    use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    let mut bytes = Vec::new();
    macro_rules! run {
        ($encoder:ident) => {
            $encoder::new(plain).read_to_end(&mut bytes).await.unwrap()
        };
    }
    match coding {
        "gzip" => run!(GzipEncoder),
        "br" => run!(BrotliEncoder),
        "deflate" => run!(ZlibEncoder),
        "zstd" => run!(ZstdEncoder),
        _ => unreachable!(),
    };
    bytes
}
async fn post(
    address: std::net::SocketAddr,
    path: &str,
    coding: &str,
    data: &[u8],
    chunked: bool,
    extra: &str,
) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let framing = if chunked {
        "Transfer-Encoding: chunked".into()
    } else {
        format!("Content-Length: {}", data.len())
    };
    stream.write_all(format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Encoding: {coding}\r\n{framing}\r\nConnection: close\r\n{extra}\r\n").as_bytes()).await.unwrap();
    if chunked {
        for part in data.chunks(7) {
            stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(part).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    } else {
        stream.write_all(data).await.unwrap();
    }
    let mut bytes = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn decoding_is_used_by_real_h1_h2_proxy_requests_and_rejects_before_forwarding() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    hyper_util::rt::TokioIo::new(stream),
                    hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        counter.fetch_add(1, Ordering::Relaxed);
                        async move {
                            assert!(!req.headers().contains_key("content-encoding"));
                            assert!(!req.headers().contains_key("content-digest"));
                            let length = req.headers()["content-length"]
                                .to_str()
                                .unwrap()
                                .parse::<usize>()
                                .unwrap();
                            let data = req.into_body().collect().await.unwrap().to_bytes();
                            assert_eq!(length, data.len());
                            Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(data)))
                        }
                    }),
                )
                .await
                .unwrap();
        }
    });
    let mut gateway = Gateway::new("decode-proxy");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_decode::bundle())
        .build()
        .unwrap();
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "origin" {{ endpoint "{backend}" }}
site "*" {{
    body-limit "1MiB"
    route "/ok" {{ decode max-ratio=1000 order=-1000; proxy upstream="origin"; }}
    route "/ratio" {{ decode max-ratio=1; proxy upstream="origin"; }}
    route "/decoded" {{ decode max-decoded="4B"; proxy upstream="origin"; }}
    route "/encoded" {{ decode max-encoded="4B"; proxy upstream="origin"; }}
    route "/guard" {{ decode order=-1000; body-limit "100B" order=1000; proxy upstream="origin"; }}
}}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let plain = b"the upstream receives decoded bytes".repeat(30);
    for coding in ["gzip", "br", "deflate", "zstd"] {
        let encoded = encode(&plain, coding).await;
        for chunked in [false, true] {
            let response = post(
                gateway.address(),
                "/ok",
                coding,
                &encoded,
                chunked,
                "Content-Digest: obsolete\r\n",
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.ends_with(std::str::from_utf8(&plain).unwrap()));
        }
        let stream = TcpStream::connect(gateway.address()).await.unwrap();
        let (mut client, connection) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            hyper_util::rt::TokioIo::new(stream),
        )
        .await
        .unwrap();
        let connection = tokio::spawn(connection);
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("http://{}/ok", gateway.address()))
            .header("content-encoding", coding)
            .body(Full::new(Bytes::from(encoded)))
            .unwrap();
        let response = client.send_request(req).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes().as_ref(),
            plain
        );
        drop(client);
        connection.abort();
        let _ = connection.await;
    }
    assert_eq!(calls.load(Ordering::Relaxed), 12);
    let compressed = encode(&plain, "gzip").await;
    assert!(
        compressed.len() < 100,
        "guard test must overflow decoded, not encoded, bytes"
    );
    for (path, coding, bytes, expected, extra) in [
        ("/ok", "gzip", b"corrupt".as_slice(), 400, ""),
        ("/ok", "unknown", compressed.as_slice(), 415, ""),
        (
            "/ok",
            "gzip",
            compressed.as_slice(),
            415,
            "Cache-Control: no-transform\r\n",
        ),
        ("/ratio", "gzip", compressed.as_slice(), 413, ""),
        ("/decoded", "gzip", compressed.as_slice(), 413, ""),
        ("/encoded", "gzip", compressed.as_slice(), 413, ""),
        ("/guard", "gzip", compressed.as_slice(), 413, ""),
    ] {
        let response = post(gateway.address(), path, coding, bytes, false, extra).await;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{response}"
        );
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        12,
        "refused requests never reached the upstream"
    );
    gateway.stop().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn reload_reuses_admission_and_duplicate_transforms_are_rejected() {
    let mut gateway = Gateway::new("decode-reload");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_decode::bundle())
        .build()
        .unwrap();
    let port = free_port();
    let config = |timeout| {
        format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ decode max-inflight=1 timeout-ms={timeout}; respond \"accepted\"; }} }}"
        )
    };
    assert!(matches!(
        gateway.apply(&config(1000)).await,
        gatel_plan::Outcome::Started { .. }
    ));
    let key = gateway
        .state
        .keys()
        .into_iter()
        .find(|key| key.kind == gatel_plan::StateKind::Directive("decode"))
        .unwrap();
    let original = gateway
        .state
        .get::<tokio::sync::Semaphore>(&key.owner, key.kind)
        .unwrap();
    let permit = original.clone().try_acquire_owned().unwrap();
    assert!(matches!(
        gateway.apply(&config(2000)).await,
        gatel_plan::Outcome::Reloaded { .. }
    ));
    let current = gateway
        .state
        .get::<tokio::sync::Semaphore>(&key.owner, key.kind)
        .unwrap();
    assert!(Arc::ptr_eq(&original, &current));
    let encoded = encode(b"hello", "gzip").await;
    assert!(
        post(gateway.address(), "/", "gzip", &encoded, false, "")
            .await
            .starts_with("HTTP/1.1 503")
    );
    drop(permit);
    assert!(
        post(gateway.address(), "/", "gzip", &encoded, false, "")
            .await
            .starts_with("HTTP/1.1 200")
    );
    let duplicate = format!(
        "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ decode; route \"/\" {{ decode; respond \"unreachable\"; }} }}"
    );
    assert!(matches!(
        gateway.apply(&duplicate).await,
        gatel_plan::Outcome::Rejected(_)
    ));
    assert!(
        post(gateway.address(), "/", "gzip", &encoded, false, "")
            .await
            .ends_with("accepted")
    );
    gateway.stop().await;
}
