//! Invalid CGI metadata must not wait for an unknown-length upload.
mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn response_head(client: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < 8192);
        head.push(client.read_u8().await.unwrap());
    }
    head
}

#[tokio::test]
async fn independent_cgi_routes_do_not_share_admission_slots() {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, uploaded) = tokio::sync::oneshot::channel();
        let (release, hold) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            first.read_to_end(&mut Vec::new()).await.unwrap();
            ready.send(()).unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            second.read_to_end(&mut Vec::new()).await.unwrap();
            second
                .write_all(b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            second.shutdown().await.unwrap();
            hold.await.unwrap();
            first
                .write_all(b"Content-Type: text/plain\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            first.shutdown().await.unwrap();
        });
        let mut gateway = Gateway::new("cgi-route-slots");
        gateway.registry = gatel_plan::Registry::builder()
            .bundle(gatel_scgi::bundle())
            .build()
            .unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let outcome = gateway
            .apply(&format!(
                r#"
global {{ http "127.0.0.1:{port}"; }}
site "*" {{
 route "/a" {{ scgi "{address}" root="." script="app.marker" concurrency=1; }}
 route "/b" {{ scgi "{address}" root="." script="app.marker" concurrency=1; }}
}}
"#
            ))
            .await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{outcome:?}"
        );
        let mut client = TcpStream::connect(gateway.address()).await.unwrap();
        client
            .write_all(b"GET /a HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        uploaded.await.unwrap();
        let busy = get(gateway.address(), "/a").await;
        assert!(busy.starts_with("HTTP/1.1 503"), "{busy}");
        let independent = get(gateway.address(), "/b").await;
        assert!(independent.starts_with("HTTP/1.1 200"), "{independent}");
        assert!(independent.ends_with("ok"));
        release.send(()).unwrap();
        let mut completed = String::new();
        client.read_to_string(&mut completed).await.unwrap();
        assert!(completed.starts_with("HTTP/1.1 200"), "{completed}");
        peer.await.unwrap();
        gateway.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn buffer_idle_timeout_releases_capacity_without_dialing_backend() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await.unwrap();
            let method = b"REQUEST_METHOD\x00GET\x00";
            assert!(bytes.windows(method.len()).any(|part| part == method));
            socket.write_all(b"Content-Type: text/plain\nContent-Length: 2\n\nok").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut gateway = Gateway::new("cgi-buffer-idle");
        gateway.registry = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle()).bundle(gatel_scgi::bundle()).build().unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ scgi \"{backend}\" root=\".\" script=\"app.marker\" buffer-unknown=#true concurrency=1 timeout=30 idle-timeout=1; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        let mut upload = TcpStream::connect(gateway.address()).await.unwrap();
        upload.write_all(b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await.unwrap();
        // This proves the handler has begun polling the upload, after admission.
        assert!(response_head(&mut upload).await.starts_with(b"HTTP/1.1 100"));
        let busy = get(gateway.address(), "/").await;
        assert!(busy.starts_with("HTTP/1.1 503"), "{busy}");
        // Withhold body bytes; the 1s idle limit must beat the 30s total limit.
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(3), response_head(&mut upload)).await.unwrap();
        assert!(timed_out.starts_with(b"HTTP/1.1 408"), "{}", String::from_utf8_lossy(&timed_out));
        let next = get(gateway.address(), "/").await;
        assert!(next.starts_with("HTTP/1.1 200") && next.ends_with("ok"), "{next}");
        peer.await.unwrap();
        gateway.stop().await;
    }).await.expect("buffer idle timeout did not release capacity");
}

#[tokio::test]
async fn invalid_metadata_is_rejected_before_body_buffering_and_backend_dial() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            // Only the final valid GET may reach this peer. Any premature dial
            // consumes its sole session and fails this independent wire check.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            socket.read_to_end(&mut raw).await.unwrap();
            let colon = raw.iter().position(|byte| *byte == b':').unwrap();
            assert!(raw[colon + 1..].starts_with(b"CONTENT_LENGTH\x000\x00"));
            let method = b"REQUEST_METHOD\x00GET\x00";
            assert!(raw.windows(method.len()).any(|part| part == method));
            socket.write_all(b"Content-Type: text/plain\nContent-Length: 2\n\nok").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut gateway = Gateway::new("cgi-early-admission");
        gateway.registry = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle()).bundle(gatel_scgi::bundle()).build().unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/*\" {{ scgi \"{backend}\" root=\".\" script=\"app.marker\" buffer-unknown=#true concurrency=1 timeout=30 pass-authorization=#true; }} }}")).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        for (path, extra) in [
            ("/%252e", ""),
            ("/", "Content-Type: text/plain\r\nContent-Type: application/json\r\n"),
            ("/", "Authorization: first\r\nAuthorization: second\r\n"),
        ] {
            let mut client = TcpStream::connect(gateway.address()).await.unwrap();
            client.write_all(format!("POST {path} HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n{extra}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
            // Deliberately do not write any chunk or the terminating zero chunk.
            let head = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 8192);
                    head.push(client.read_u8().await.unwrap());
                }
                head
            }).await.expect("invalid metadata waited for request body");
            assert!(head.starts_with(b"HTTP/1.1 400"), "{}", String::from_utf8_lossy(&head));
        }
        let valid = get(gateway.address(), "/").await;
        assert!(valid.starts_with("HTTP/1.1 200") && valid.ends_with("ok"), "{valid}");
        peer.await.unwrap();
        gateway.stop().await;
    }).await.expect("CGI admission retained capacity or contacted backend early");
}
