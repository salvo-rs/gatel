use std::net::TcpListener;
use std::sync::Arc;

use super::*;

fn options(address: SocketAddr, kind: Kind) -> Options {
    Options {
        address,
        kind,
        path: "/readyz".into(),
        host: "localhost".into(),
        timeout_ms: 1000,
    }
}

#[test]
fn probes_reject_nonlocal_targets_and_request_injection() {
    for input in ["0.0.0.0:8080", "192.0.2.1:8080", "[::]:8080", "127.0.0.1:0"] {
        assert!(address(input).is_err(), "{input}");
    }
    assert!(address("[::1]:8080").is_ok());
    for input in [
        "http://example.com/",
        "//example.com/",
        "/a\r\nX: a",
        "/with space",
        "/#fragment",
    ] {
        assert!(path(input).is_err(), "{input}");
    }
    assert!(host("localhost\r\nInjected: yes").is_err());
    assert!(host("example.test:8080").is_ok());
}

#[test]
fn responses_must_be_the_requested_health_terminal_not_an_arbitrary_200() {
    assert!(accepted(b"HTTP/1.1 200 OK\r\nX-Gatel-Health: ready", Kind::Ready).is_ok());
    assert!(accepted(b"HTTP/1.0 200 OK\r\nx-gatel-health: live", Kind::Live).is_ok());
    for response in [
        b"HTTP/1.1 200 OK".as_slice(),
        b"HTTP/1.1 503 Service Unavailable\r\nx-gatel-health: ready",
        b"HTTP/1.1 302 Found\r\nx-gatel-health: ready",
        b"HTTP/1.1 200 OK\r\nx-gatel-health: live",
        b"HTTP/1.1 200 OK\r\nx-gatel-health: ready\r\nx-gatel-health: ready",
        b"HTTP/1.1 200 OK\r\ninvalid header",
    ] {
        assert!(accepted(response, Kind::Ready).is_err());
    }
}

#[test]
fn header_budget_and_one_deadline_bound_unresponsive_and_dribbling_peers() {
    for dribble in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = options(listener.local_addr().unwrap(), Kind::Ready);
        config.timeout_ms = 80;
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            if dribble {
                for _ in 0..40 {
                    if stream.write_all(b"H").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                let _ = stream.write_all(&vec![b'x'; MAX_HEAD + 1]);
            }
        });
        let started = Instant::now();
        assert!(check(&config).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn actual_runtime_reports_degraded_ready_draining_and_liveness_without_loading_disk() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!("gatel-probe-{}-{serial}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = directory.join("gatel.kdl");
    std::fs::write(&config, format!("global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/readyz\" {{ health \"ready\"; }}; route \"/livez\" {{ health \"live\"; }}; }}")).unwrap();
    let app = Arc::new(crate::app::App::new(config.clone()).unwrap());
    app.binds.adopt(listener).unwrap();
    assert!(matches!(app.reload().await, gatel_plan::Outcome::Started { .. }));
    async fn probe(config: Options) -> Result<(), &'static str> {
        tokio::task::spawn_blocking(move || check(&config)).await.unwrap()
    }
    assert!(probe(options(address, Kind::Ready)).await.is_ok());
    std::fs::write(&config, "invalid-node;").unwrap();
    assert!(matches!(app.reload().await, gatel_plan::Outcome::Rejected(_)));
    assert!(
        probe(options(address, Kind::Ready)).await.is_ok(),
        "last good runtime must stay ready"
    );
    app.telemetry.health.upstream("synthetic-origin", false);
    assert!(
        probe(options(address, Kind::Ready)).await.is_ok(),
        "shared upstream failure is degraded"
    );
    app.telemetry.health.draining();
    assert!(probe(options(address, Kind::Ready)).await.is_err());
    let mut live = options(address, Kind::Live);
    live.path = "/livez".into();
    assert!(probe(live).await.is_ok());
    app.shutdown().await;
    assert!(probe(options(address, Kind::Ready)).await.is_err());
    std::fs::remove_dir_all(directory).unwrap();
}
