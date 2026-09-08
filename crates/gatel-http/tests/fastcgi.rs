mod support;
use std::collections::BTreeMap;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn record(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut prefix = [0; 8];
    stream.read_exact(&mut prefix).await.unwrap();
    assert_eq!(&prefix[..1], &[1]);
    assert_eq!(&prefix[2..4], &[0, 1]);
    let mut bytes = vec![0; u16::from_be_bytes([prefix[4], prefix[5]]) as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    let mut padding = vec![0; prefix[6] as usize];
    stream.read_exact(&mut padding).await.unwrap();
    (prefix[1], bytes)
}
async fn send(stream: &mut TcpStream, kind: u8, bytes: &[u8]) {
    let len = (bytes.len() as u16).to_be_bytes();
    stream
        .write_all(&[1, kind, 0, 1, len[0], len[1], 0, 0])
        .await
        .unwrap();
    stream.write_all(bytes).await.unwrap();
}
fn decode_params(mut bytes: &[u8]) -> BTreeMap<String, String> {
    fn length(bytes: &mut &[u8]) -> usize {
        let first = bytes[0];
        if first < 128 {
            *bytes = &bytes[1..];
            first as usize
        } else {
            let value = u32::from_be_bytes(bytes[..4].try_into().unwrap()) & 0x7fffffff;
            *bytes = &bytes[4..];
            value as usize
        }
    }
    let mut result = BTreeMap::new();
    while !bytes.is_empty() {
        let name = length(&mut bytes);
        let value = length(&mut bytes);
        result.insert(
            String::from_utf8(bytes[..name].to_vec()).unwrap(),
            String::from_utf8(bytes[name..name + value].to_vec()).unwrap(),
        );
        bytes = &bytes[name + value..];
    }
    result
}

#[tokio::test]
async fn chunked_buffer_is_opt_in_bounded_and_releases_capacity_after_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut gateway = Gateway::new("fastcgi-buffer");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("index.php"), "<?php").unwrap();
    let port = free_port();
    let config = |enabled: bool| {
        format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ fastcgi "{address}" root="." script="index.php" buffer-unknown=#{enabled} max-buffer=5 concurrency=1 timeout=1; }} }}"#
        )
    };
    gateway.apply(&config(false)).await;
    let chunked = "POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    let response = request(gateway.address(), &format!("{chunked}5\r\nhello\r\n0\r\n\r\n")).await;
    assert!(response.starts_with("HTTP/1.1 411"), "{response}");
    gateway.apply(&config(true)).await;
    let response = request(gateway.address(), &format!("{chunked}6\r\nhello!\r\n0\r\n\r\n")).await;
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    let mut slow = TcpStream::connect(gateway.address()).await.unwrap();
    slow.write_all(format!("{chunked}1\r\nx\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        slow.read_to_string(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    // None of the rejected requests may reach the backend.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(record(&mut stream).await.0, 1);
        let mut params = Vec::new();
        loop {
            let (kind, data) = record(&mut stream).await;
            assert_eq!(kind, 4);
            if data.is_empty() {
                break;
            }
            params.extend(data);
        }
        assert_eq!(decode_params(&params)["CONTENT_LENGTH"], "5");
        let mut body = Vec::new();
        loop {
            let (kind, data) = record(&mut stream).await;
            assert_eq!(kind, 5);
            if data.is_empty() {
                break;
            }
            body.extend(data);
        }
        assert_eq!(body, b"hello");
        send(
            &mut stream,
            6,
            b"Content-Type: text/plain\nContent-Length: 2\n\nok",
        )
        .await;
        send(&mut stream, 3, &[0; 8]).await;
    });
    let response = request(
        gateway.address(),
        &format!("{chunked}2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok"), "{response}");
    peer.await.unwrap();
    gateway.stop().await;
}

#[tokio::test]
async fn host_and_middleware_body_limits_remain_413_through_fastcgi_buffering() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut gateway = Gateway::new("fastcgi-guard-errors");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("index.php"), "<?php").unwrap();
    let port = free_port();
    for (global, guard) in [("limits body=4;", ""), ("", "body-limit 4;")] {
        let outcome = gateway.apply(&format!(r#"global {{ http "127.0.0.1:{port}"; {global} }}
site "*" {{ route "/*" {{ {guard} fastcgi "{address}" root="." script="index.php" buffer-unknown=#true; }} }}"#)).await;
        assert!(
            matches!(
                outcome,
                gatel_plan::Outcome::Started { .. } | gatel_plan::Outcome::Reloaded { .. }
            ),
            "{outcome:?}"
        );
        let response = request(gateway.address(), "POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    gateway.stop().await;
}

#[tokio::test]
async fn late_application_failure_cannot_satisfy_http_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        record(&mut stream).await;
        loop {
            if record(&mut stream).await.1.is_empty() {
                break;
            }
        }
        record(&mut stream).await;
        send(
            &mut stream,
            6,
            b"Content-Type: text/plain\nContent-Length: 5\n\nhello",
        )
        .await;
        wait.await.unwrap();
        send(&mut stream, 6, b"").await;
        send(&mut stream, 3, &[0, 0, 0, 7, 0, 0, 0, 0]).await;
    });
    let mut gateway = Gateway::new("fastcgi-late-error");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("index.php"), "<?php").unwrap();
    let port = free_port();
    gateway
        .apply(&format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ fastcgi "{address}" root="." script="index.php"; }} }}"#
        ))
        .await;
    let mut client = TcpStream::connect(gateway.address()).await.unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(client.read_u8().await.unwrap());
    }
    let mut first = [0; 4];
    tokio::time::timeout(std::time::Duration::from_secs(1), client.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&first, b"hell", "response must still stream before END_REQUEST");
    release.send(()).unwrap();
    let mut tail = Vec::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), client.read_to_end(&mut tail)).await;
    assert!(result.is_ok()); // EOF or reset, but never the withheld fifth byte.
    assert!(tail.is_empty());
    peer.await.unwrap();
    gateway.stop().await;
}

#[tokio::test]
#[ignore = "requires explicit GATEL_PHP_FPM_ADDR and example scripts mounted at /app in FPM"]
async fn real_php_fpm_get_post_and_head() {
    let address = std::env::var("GATEL_PHP_FPM_ADDR").expect("explicit test FPM address");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/fastcgi/examples")
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_string();
    let mut gateway = Gateway::new("php-fpm-interop");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ fastcgi "{address}" root="{root}" script="index.php" remote-root="/app"; }} }}"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for method in ["GET", "POST", "HEAD"] {
        let (path, body) = if method == "POST" {
            ("/?echo=1", "real-upload")
        } else {
            ("/", "")
        };
        let response = request(gateway.address(), &format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        if method == "POST" {
            assert!(response.ends_with("real-upload"), "{response}");
        } else if method != "HEAD" {
            assert!(response.contains("Hello from FastCGI"), "{response}");
        } else {
            assert!(response.ends_with("\r\n\r\n"), "{response}");
        }
    }
    gateway.stop().await;
}

#[tokio::test]
async fn real_tcp_terminal_maps_environment_streams_and_handles_head() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        for head in [false, true] {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(record(&mut stream).await.0, 1);
            let mut params = Vec::new();
            loop {
                let (kind, data) = record(&mut stream).await;
                assert_eq!(kind, 4);
                if data.is_empty() {
                    break;
                }
                params.extend(data);
            }
            let env = decode_params(&params);
            assert!(env["SCRIPT_FILENAME"].ends_with("index.php"));
            assert_eq!(env["SCRIPT_NAME"], "/index.php");
            assert_eq!(env["PATH_INFO"], "/item");
            assert_eq!(env["QUERY_STRING"], "x=1");
            assert_eq!(env["SERVER_PROTOCOL"], "HTTP/1.1");
            assert_eq!(env["SERVER_NAME"], "example.test");
            assert!(!env.contains_key("HTTP_PROXY"));
            assert!(!env.contains_key("HTTP_AUTHORIZATION"));
            assert!(!env.contains_key("HTTP_X_REMOVE"));
            let mut body = Vec::new();
            loop {
                let (kind, data) = record(&mut stream).await;
                assert_eq!(kind, 5);
                if data.is_empty() {
                    break;
                }
                body.extend(data);
            }
            assert_eq!(body, if head { b"".as_slice() } else { b"hello".as_slice() });
            send(
                &mut stream,
                6,
                b"Content-Type: text/plain\r\nContent-Length: 5\r\n\r\n",
            )
            .await;
            if !head {
                send(&mut stream, 6, b"world").await;
            }
            send(&mut stream, 6, b"").await;
            send(&mut stream, 3, &[0; 8]).await;
        }
    });
    let mut gateway = Gateway::new("fastcgi-terminal");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_fastcgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("index.php"), "<?php echo 'world';").unwrap();
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ fastcgi "{address}" root="." script="index.php" server-name="example.test"; }} }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let response = request(gateway.address(), "POST /index.php/item?x=1 HTTP/1.1\r\nHost: untrusted\r\nContent-Length: 5\r\nProxy: attacker\r\nAuthorization: secret\r\nX-Remove: secret\r\nConnection: close, x-remove\r\n\r\nhello").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("world"), "{response}");
    let response = request(
        gateway.address(),
        "HEAD /index.php/item?x=1 HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("\r\n\r\n"));
    peer.await.unwrap();
    gateway.stop().await;
}
