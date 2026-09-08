//! Exercise the optional protocols through real Unix domain sockets and HTTP.
#![cfg(unix)]

mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

async fn fastcgi_record(socket: &mut UnixStream) -> (u8, Vec<u8>) {
    let mut header = [0; 8];
    socket.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 1);
    assert_eq!(&header[2..4], &[0, 1]);
    let mut content = vec![0; u16::from_be_bytes([header[4], header[5]]) as usize];
    socket.read_exact(&mut content).await.unwrap();
    let mut padding = vec![0; header[6] as usize];
    socket.read_exact(&mut padding).await.unwrap();
    (header[1], content)
}

async fn fastcgi_reply(socket: &mut UnixStream, kind: u8, bytes: &[u8]) {
    let length = u16::try_from(bytes.len()).unwrap().to_be_bytes();
    socket
        .write_all(&[1, kind, 0, 1, length[0], length[1], 0, 0])
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
}

async fn scenario(protocol: &str) {
    let mut gateway = Gateway::new("uds");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_scgi::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ {protocol} "unix:cgi.sock" root="." script="app.marker" concurrency=1; }} }}"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    // Socket availability is a request-time concern. A failed dial must release
    // the sole permit, and the same generation must recover without a reload.
    let unavailable = get(gateway.address(), "/").await;
    assert!(unavailable.starts_with("HTTP/1.1 502"), "{unavailable}");
    let listener = UnixListener::bind(gateway.directory.join("cgi.sock")).unwrap();
    let is_scgi = protocol == "scgi";
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        if is_scgi {
            let mut length = String::new();
            loop {
                let byte = socket.read_u8().await.unwrap();
                if byte == b':' {
                    break;
                }
                assert!(byte.is_ascii_digit() && length.len() < 6);
                length.push(char::from(byte));
            }
            let size: usize = length.parse().unwrap();
            assert!(size < 65536);
            let mut environment = vec![0; size];
            socket.read_exact(&mut environment).await.unwrap();
            assert!(environment.starts_with(b"CONTENT_LENGTH\x005\x00"));
            assert!(environment.windows(7).any(|v| v == b"SCGI\x001\x00"));
            assert_eq!(socket.read_u8().await.unwrap(), b',');
            let mut body = Vec::new();
            // SCGI must half-close its upload without closing the response direction.
            socket.read_to_end(&mut body).await.unwrap();
            assert_eq!(body, b"hello");
            socket
                .write_all(b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        } else {
            let (kind, begin) = fastcgi_record(&mut socket).await;
            assert_eq!(kind, 1);
            assert_eq!(&begin[..2], &[0, 1]);
            let mut params = Vec::new();
            loop {
                let (kind, bytes) = fastcgi_record(&mut socket).await;
                assert_eq!(kind, 4);
                if bytes.is_empty() {
                    break;
                }
                params.extend(bytes);
            }
            assert!(!params.is_empty());
            let mut body = Vec::new();
            loop {
                let (kind, bytes) = fastcgi_record(&mut socket).await;
                assert_eq!(kind, 5);
                if bytes.is_empty() {
                    break;
                }
                body.extend(bytes);
            }
            assert_eq!(body, b"hello");
            fastcgi_reply(
                &mut socket,
                6,
                b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
            )
            .await;
            fastcgi_reply(&mut socket, 6, b"").await;
            fastcgi_reply(&mut socket, 3, &[0; 8]).await;
        }
    });
    let response = request(
        gateway.address(),
        "POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok"), "{response}");
    peer.await.unwrap();
    gateway.stop().await;
}

#[tokio::test]
async fn scgi_unix_socket_upload_half_close_and_response() {
    tokio::time::timeout(std::time::Duration::from_secs(10), scenario("scgi"))
        .await
        .unwrap();
}

#[tokio::test]
async fn fastcgi_unix_socket_records_and_completion() {
    tokio::time::timeout(std::time::Duration::from_secs(10), scenario("fastcgi"))
        .await
        .unwrap();
}
