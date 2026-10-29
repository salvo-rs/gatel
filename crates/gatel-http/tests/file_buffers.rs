//! Owned file buffers through consumers that retain or transform body frames.
mod support;

use http_body_util::BodyExt as _;
use support::*;
use tokio::io::AsyncReadExt as _;

#[tokio::test]
async fn pooled_and_streamed_files_survive_encoding_and_cache_reuse() {
    let gateway = Gateway::new("file-buffer-consumers");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    let sizes = [32768, 32769, 65536, 65537, 1024 * 1024 + 17];
    for (id, length) in sizes.into_iter().enumerate() {
        let bytes: Vec<_> = (0..length)
            .map(|offset| b'a' + ((offset + id) % 26) as u8)
            .collect();
        std::fs::write(root.join(format!("{id}.txt")), bytes).unwrap();
    }
    let port = free_port();
    let outcome = gateway.apply(&format!(
        "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ cache default-ttl=\"60s\" max-entry=\"2MiB\"; encode \"gzip\" min-size=1; file-server root=\"{}\"; }} }}",
        root.display().to_string().replace('\\', "/")
    )).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let io =
            hyper_util::rt::TokioIo::new(tokio::net::TcpStream::connect(gateway.address()).await.unwrap());
        let (mut client, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
        let driver = tokio::spawn(connection);
        for (id, length) in sizes.into_iter().enumerate() {
            let expected: Vec<_> = (0..length)
                .map(|offset| b'a' + ((offset + id) % 26) as u8)
                .collect();
            for encoding in ["identity", "gzip"] {
                for repeat in 0..2 {
                    let response = client
                        .send_request(
                            http::Request::builder()
                                .uri(format!("/{id}.txt"))
                                .header("host", "example.com")
                                .header("accept-encoding", encoding)
                                .body(http_body_util::Empty::<bytes::Bytes>::new())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(response.status(), http::StatusCode::OK);
                    if repeat == 1 {
                        assert!(
                            response.headers()["cache-status"]
                                .to_str()
                                .unwrap()
                                .contains("hit;")
                        );
                    }
                    if encoding == "gzip" {
                        assert_eq!(response.headers()["content-encoding"], "gzip");
                    } else {
                        assert!(!response.headers().contains_key("content-encoding"));
                    }
                    let bytes = response.into_body().collect().await.unwrap().to_bytes();
                    let decoded = if encoding == "gzip" {
                        let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(bytes.as_ref());
                        let mut decoded = Vec::new();
                        decoder.read_to_end(&mut decoded).await.unwrap();
                        decoded
                    } else {
                        bytes.to_vec()
                    };
                    assert_eq!(decoded, expected);
                }
            }
        }
        drop(client);
        driver.await.unwrap().unwrap();
    })
    .await
    .expect("file consumers did not complete");
}

#[tokio::test]
async fn http2_preserves_whole_files_and_ranges_across_the_inline_boundary() {
    let gateway = Gateway::new("file-buffer-h2");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    let expected: Vec<_> = (0..1024 * 1024 + 17).map(|offset| (offset % 251) as u8).collect();
    std::fs::write(root.join("data.bin"), &expected).unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!(
        "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\"; }} }}",
        root.display().to_string().replace('\\', "/")
    )).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let io =
            hyper_util::rt::TokioIo::new(tokio::net::TcpStream::connect(gateway.address()).await.unwrap());
        let (mut client, connection) =
            hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), io)
                .await
                .unwrap();
        let driver = tokio::spawn(connection);
        for range in [
            None,
            Some((0, 65535)),
            Some((0, 65536)),
            Some((13, 65548)),
            Some((13, 65549)),
        ] {
            let mut request = http::Request::builder().uri("http://example.com/data.bin");
            if let Some((start, end)) = range {
                request = request.header("range", format!("bytes={start}-{end}"));
            }
            let response = client
                .send_request(
                    request
                        .body(http_body_util::Empty::<bytes::Bytes>::new())
                        .unwrap(),
                )
                .await
                .unwrap();
            let (start, end) = range.unwrap_or((0, expected.len() - 1));
            assert_eq!(response.status(), if range.is_some() { 206 } else { 200 });
            assert_eq!(
                response.headers()["content-length"],
                (end - start + 1).to_string()
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(bytes.as_ref(), &expected[start..=end]);
        }
        drop(client);
        driver.await.unwrap().unwrap();
    })
    .await
    .expect("HTTP/2 file delivery did not complete");
}
