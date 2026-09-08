mod support;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn same_backend_routes_keep_independent_response_deadlines() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let (release, held) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut held = Some(held);
            for first in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                    assert!(head.len() < 8192);
                }
                if first {
                    held.take().unwrap().await.unwrap();
                    // The timed-out request's transport may already be closed.
                } else {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .await
                        .unwrap();
                }
            }
        });
        let gateway = Gateway::new("inline-policies");
        let port = free_port();
        // Slow comes first: the old address-only deduplication would give the
        // fast route this policy and exceed its two-second test bound.
        let config = format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{
 route "/version" {{ respond "original"; }};
 route "/slow" {{ proxy {{ endpoint "{backend}"; timeouts response-head="5s"; }}; }};
 route "/fast" {{ proxy {{ endpoint "{backend}"; timeouts response-head="100ms"; }}; }};
}}"#
        );
        assert!(matches!(
            gateway.apply(&config).await,
            gatel_plan::Outcome::Started { .. }
        ));
        assert_eq!(gateway.store.current().unwrap().plan.spec.upstreams.len(), 2);
        let fast = tokio::time::timeout(std::time::Duration::from_secs(2), get(gateway.address(), "/fast"))
            .await
            .expect("fast route inherited the slow route's response deadline");
        assert!(fast.starts_with("HTTP/1.1 504"), "{fast}");
        release.send(()).unwrap();
        let slow = get(gateway.address(), "/slow").await;
        assert!(slow.starts_with("HTTP/1.1 200"), "{slow}");
        assert!(slow.ends_with("ok"));
        peer.await.unwrap();
        let previous = gateway.store.current().unwrap();
        let invalid = config.replace(
            "timeouts response-head=\"5s\";",
            "timeouts response-head=\"5s\"; timeouts response-head=\"1s\";",
        );
        assert!(matches!(
            gateway.apply(&invalid).await,
            gatel_plan::Outcome::Rejected(_)
        ));
        assert!(std::sync::Arc::ptr_eq(
            &previous,
            &gateway.store.current().unwrap()
        ));
        assert!(get(gateway.address(), "/version").await.ends_with("original"));
        gateway.stop().await;
    })
    .await
    .unwrap();
}
