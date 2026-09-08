mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn scgi_response_pins_old_generation_and_failed_prepare_keeps_lkg() {
    tokio::time::timeout(std::time::Duration::from_secs(8), reload_scenario())
        .await
        .unwrap();
}

#[tokio::test]
async fn configured_idle_timeout_applies_before_no_body_response_is_published() {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, hold) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).await.unwrap();
            socket
                .write_all(b"Content-Type: text/plain\nContent-Length: 0\n\n")
                .await
                .unwrap();
            // Withhold EOF beyond idle-timeout. Do not accidentally finish the protocol.
            hold.await.unwrap();
        });
        let mut gateway = Gateway::new("scgi-idle-config");
        gateway.registry = gatel_plan::Registry::builder()
            .bundle(gatel_bundle_std::bundle())
            .bundle(gatel_scgi::bundle())
            .build()
            .unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "marker").unwrap();
        let port = free_port();
        let outcome = gateway
            .apply(&format!(
                r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ scgi "{address}" root="." script="app.marker" timeout=30 idle-timeout=1; }} }}"#
            ))
            .await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{outcome:?}"
        );
        let response = request(
            gateway.address(),
            "GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        release.send(()).unwrap();
        peer.await.unwrap();
        gateway.stop().await;
    })
    .await
    .unwrap();
}

async fn reload_scenario() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        socket.read_to_end(&mut request).await.unwrap();
        socket
            .write_all(b"Content-Type: text/plain\nContent-Length: 3\n\nold")
            .await
            .unwrap();
        wait.await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let mut gateway = Gateway::new("scgi-reload");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_scgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("app.marker"), "deployment marker").unwrap();
    let port = free_port();
    let source = |script: &str, marker: &str| {
        format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/version" {{ respond "{marker}"; }}; route "/*" {{ scgi "{address}" root="." script="{script}"; }} }}"#
        )
    };
    assert!(matches!(
        gateway.apply(&source("app.marker", "old-config")).await,
        gatel_plan::Outcome::Started { .. }
    ));
    let old = gateway.store.current().unwrap();
    let weak = std::sync::Arc::downgrade(&old);
    let number = old.number;
    drop(old);
    let mut client = tokio::net::TcpStream::connect(gateway.address()).await.unwrap();
    client
        .write_all(b"GET /app HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(client.read_u8().await.unwrap());
    }
    assert!(head.starts_with(b"HTTP/1.1 200"));
    let mut partial = [0; 2];
    client.read_exact(&mut partial).await.unwrap();
    assert_eq!(&partial, b"ol");
    let rejected = gateway.apply(&source("missing.marker", "bad-config")).await;
    assert!(
        matches!(rejected, gatel_plan::Outcome::PrepareFailed(_)),
        "{rejected:?}"
    );
    assert_eq!(gateway.store.current().unwrap().number, number);
    assert!(get(gateway.address(), "/version").await.ends_with("old-config"));
    assert!(matches!(
        gateway.apply(&source("app.marker", "new-config")).await,
        gatel_plan::Outcome::Reloaded { .. }
    ));
    assert!(weak.upgrade().is_some(), "in-flight CGI lost its generation");
    assert!(get(gateway.address(), "/version").await.ends_with("new-config"));
    release.send(()).unwrap();
    let mut tail = Vec::new();
    client.read_to_end(&mut tail).await.unwrap();
    assert_eq!(tail, b"d");
    peer.await.unwrap();
    drop(client);
    while weak.upgrade().is_some() {
        tokio::task::yield_now().await;
    }
    gateway.stop().await;
}

#[tokio::test]
async fn http_scgi_chunked_and_head_use_shared_mapping_and_lifecycle() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            for head in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                socket.read_to_end(&mut raw).await.unwrap();
                let colon = raw.iter().position(|b| *b == b':').unwrap();
                let size: usize = std::str::from_utf8(&raw[..colon]).unwrap().parse().unwrap();
                let fields = raw[colon+1..colon+1+size].split(|b| *b == 0).collect::<Vec<_>>();
                assert_eq!(fields[0], b"CONTENT_LENGTH");
                assert_eq!(fields[1], if head { b"0" } else { b"5" });
                assert_eq!(&fields[2..4], &[b"SCGI".as_slice(), b"1".as_slice()]);
                assert!(fields.windows(2).any(|pair| pair == [b"PATH_INFO".as_slice(), b"/item".as_slice()]));
                assert!(!fields.contains(&b"HTTP_AUTHORIZATION".as_slice()));
                if !head {
                    assert!(!fields.contains(&b"CONTENT_TYPE".as_slice()));
                    assert!(!fields.contains(&b"HTTP_X_INTERNAL".as_slice()));
                    assert!(fields.windows(2).any(|pair| pair == [b"HTTP_COOKIE".as_slice(), b"a=1; b=2".as_slice()]));
                    assert!(fields.windows(2).any(|pair| pair == [b"HTTP_X_LIST".as_slice(), b"first, second".as_slice()]));
                }
                assert_eq!(raw[colon+1+size], b',');
                assert_eq!(&raw[colon+2+size..], if head { b"".as_slice() } else { b"hello".as_slice() });
                socket.write_all(b"Content-Type: text/plain\nContent-Length: 5\n\n").await.unwrap();
                if !head { socket.write_all(b"world").await.unwrap(); }
                socket.shutdown().await.unwrap();
            }
        });
        let mut gateway = Gateway::new("scgi-terminal");
        gateway.registry = gatel_plan::Registry::builder().bundle(gatel_bundle_std::bundle()).bundle(gatel_scgi::bundle()).build().unwrap();
        std::fs::write(gateway.directory.join("app.marker"), "deployment marker").unwrap();
        let port = free_port();
        let outcome = gateway.apply(&format!(r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ scgi "{address}" root="." script="app.marker" script-name="/app" buffer-unknown=#true; }} }}"#)).await;
        assert!(matches!(outcome, gatel_plan::Outcome::Started { .. }), "{outcome:?}");
        // The peer expects only the two valid requests. An invalid mapping
        // must not connect before returning 400 or consume its next session.
        let invalid = request(gateway.address(), "POST /app/item HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\nContent-Type: text/plain\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n").await;
        assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");
        let response = request(gateway.address(), "POST /app/item HTTP/1.1\r\nHost: test\r\nAuthorization: secret\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\nX-Internal: private\r\nCookie: a=1\r\nCookie: b=2\r\nX-List: first\r\nX-List: second\r\nConnection: close, Content-Type, X-Internal\r\n\r\n5\r\nhello\r\n0\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("world"), "{response}");
        let response = request(gateway.address(), "HEAD /app/item HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("\r\n\r\n"));
        peer.await.unwrap();
        gateway.stop().await;
    }).await.unwrap();
}
