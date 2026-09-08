use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[tokio::test]
async fn timeout_and_caller_cancellation_close_both_http_driver_transports() {
    for protocol in [UpstreamProtocol::Http1, UpstreamProtocol::Http2] {
        for cancel in [false, true] {
            tokio::time::timeout(Duration::from_secs(5), async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let (started, ready) = tokio::sync::oneshot::channel();
                let peer = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    if protocol == UpstreamProtocol::Http2 {
                        // Empty server SETTINGS. Deliberately never answer HEADERS.
                        socket.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
                    }
                    let mut initial = [0u8; 4096];
                    assert!(socket.read(&mut initial).await.unwrap() > 0);
                    started.send(()).unwrap();
                    let mut rest = Vec::new();
                    // EOF or connection reset both prove the probe closed transport.
                    let _ = socket.read_to_end(&mut rest).await;
                });
                let task = tokio::spawn(async move {
                    probe(
                        address,
                        &HealthKind::Http {
                            path: "/".into(),
                            status: 200,
                            host: None,
                        },
                        protocol,
                        if cancel {
                            Duration::from_secs(60)
                        } else {
                            Duration::from_millis(100)
                        },
                    )
                    .await
                });
                ready.await.unwrap();
                if cancel {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                } else {
                    assert!(task.await.unwrap().is_err());
                }
                peer.await.unwrap();
            })
            .await
            .expect("cancelled probe retained its driver connection");
        }
    }
}
