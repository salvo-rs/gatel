use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::admin::AdminApi;
use crate::admin::server::AdminServer;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

async fn probe(app: &App, path: &str) -> String {
    let address = app.server.active_addresses().await[0];
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: health.invalid\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    client
        .take(128 * 1024)
        .read_to_string(&mut response)
        .await
        .unwrap();
    response
}

fn health_config(port: u16) -> String {
    format!(
        "upstream \"runtime/http\" {{ endpoint \"127.0.0.1:{port}\"; }}\nsite \"health.invalid\" {{\nroute \"/readyz\" {{ health \"ready\"; }}\nroute \"/livez\" {{ health \"live\"; }}\nroute \"/metrics\" {{ metrics; }}\n}}"
    )
}

#[tokio::test]
async fn disconnected_http_request_and_admin_shutdown_do_not_cancel_checkpoint() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::with_sources([SourceId::file("main"), SourceId::admin()]);
        let app = &fixture.app;
        assert!(matches!(app.reload_checkpoint_if(app.store.status().version(), fixture.snapshot.clone(),
            fixture.writer.clone()).await, Ok(Outcome::Started { .. })));
        let api = Arc::new(AdminApi::with_checkpoint(app, zeroize::Zeroizing::new(TOKEN.into()),
            fixture.writer.clone()).unwrap());
        let server = AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), api.clone()).unwrap();
        let path = fixture.directory.join("checkpoint.bin");
        let before = std::fs::read(&path).unwrap();
        let lifecycle = app.lifecycle.lock().await;
        let mut client = tokio::net::TcpStream::connect(server.address()).await.unwrap();
        let body = "upstream \"runtime/disconnected\" { endpoint \"127.0.0.1:2\"; }";
        client.write_all(format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}",
            app.store.status().version().etag(), body.len()).as_bytes()).await.unwrap();
        while app.store.current().unwrap().source_snapshot().get(&SourceId::admin()).is_none() {
            tokio::task::yield_now().await;
        }
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let sink = fixture.writer.clone();
        let obstruction = tokio::task::spawn_blocking(move || {
            let _guard = sink.lock().unwrap();
            locked.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready.await.unwrap();
        drop(lifecycle);
        while app.activated.load(Ordering::Acquire) < app.store.current().unwrap().number.number() {
            tokio::task::yield_now().await;
        }
        assert_serving(app).await;
        let mut received = [0; 1];
        assert_eq!(client.try_read(&mut received).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(client);
        // This stops the real HTTP connection tasks while disk completion is
        // deliberately still blocked. The application owns accepted work.
        server.shutdown().await.unwrap();
        assert!(app.reload_execution.try_lock().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        release.send(()).unwrap();
        obstruction.await.unwrap();
        app.submissions.drained().await;
        assert_ne!(std::fs::read(&path).unwrap(), before);
        fixture.writer.lock().unwrap().ensure_ready().unwrap();
        let capture = app.store.capture_configuration();
        app.shutdown().await;
        drop(api);
        drop(fixture.writer);
        let (_reader, snapshots) = CheckpointWriter::open(&fixture.directory, "fixture", &app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(gatel_model::snapshot_codec::encode(&serving).unwrap(),
            gatel_model::snapshot_codec::encode(capture.committed().unwrap().source_snapshot()).unwrap());
        assert_eq!(gatel_model::snapshot_codec::encode(&observed).unwrap(),
            gatel_model::snapshot_codec::encode(capture.observed_snapshot()).unwrap());
    }).await.unwrap();
}

pub(super) async fn put(address: std::net::SocketAddr, tag: &str, body: &str) -> String {
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut response = String::new();
    client
        .take(16 * 1024)
        .read_to_string(&mut response)
        .await
        .unwrap();
    response
}

async fn admin_status(address: std::net::SocketAddr) -> serde_json::Value {
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!(
                "GET /v1/config/status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    client
        .take(16 * 1024)
        .read_to_string(&mut response)
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("content-type: application/json"));
    assert!(!response.contains(TOKEN));
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn http_checkpoint_receipt_distinguishes_saved_from_published_but_unknown() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::with_sources([SourceId::file("main"), SourceId::admin()]);
        let app = &fixture.app;
        assert!(matches!(
            app.reload_checkpoint_if(
                app.store.status().version(),
                fixture.snapshot.clone(),
                fixture.writer.clone()
            )
            .await,
            Ok(Outcome::Started { .. })
        ));
        let api = Arc::new(
            AdminApi::with_checkpoint(app, zeroize::Zeroizing::new(TOKEN.into()), fixture.writer.clone())
                .unwrap(),
        );
        let server =
            AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), api.clone()).unwrap();
        let body = health_config(2);
        let path = fixture.directory.join("checkpoint.bin");
        let initial_bytes = std::fs::read(&path).unwrap();
        let initial = app.store.status().version().etag();
        let response = put(server.address(), &initial, &body).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let ready = probe(app, "/readyz").await;
        assert!(
            ready.starts_with("HTTP/1.1 200") && ready.ends_with("ready\n"),
            "{ready}"
        );
        let saved = std::fs::read(&path).unwrap();
        let status = admin_status(server.address()).await;
        assert_eq!(status["write_mode"], "checkpoint");
        assert_eq!(status["persistence_unknown"], false);
        assert_eq!(status["health"]["ready"], true);
        assert_ne!(saved, initial_bytes);
        let committed = app.store.capture_configuration();
        let response = put(server.address(), &initial, &body).await;
        assert!(response.starts_with("HTTP/1.1 412"), "{response}");
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        // Preserve the last confirmed checkpoint while forcing a real save
        // failure at its pathname. All files belong to this private fixture.
        let backup = fixture.directory.join("confirmed.bin");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        let changed = health_config(3);
        let response = put(server.address(), &app.store.status().version().etag(), &changed).await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        let health = app.telemetry.health.report();
        assert_eq!(health.state, gatel_observe::health::State::NotReady);
        assert!(
            health
                .reasons
                .contains(&gatel_observe::health::Reason::PersistenceUnknown)
        );
        assert!(app.telemetry.health.alive());
        let status = admin_status(server.address()).await;
        assert_eq!(status["persistence_unknown"], true);
        assert_eq!(status["published"], true);
        assert_eq!(status["health"]["state"], "not_ready");
        assert_eq!(status["configuration_etag"], app.store.status().version().etag());
        let metrics = probe(app, "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
        assert!(metrics.contains("gatel_ready 0\n"));
        assert!(metrics.contains("gatel_health_reason{reason=\"persistence_unknown\"} 1\n"));
        assert!(!metrics.contains(TOKEN));
        let ready = probe(app, "/readyz").await;
        assert!(ready.starts_with("HTTP/1.1 503"), "{ready}");
        assert!(ready.contains("cache-control: no-store"), "{ready}");
        assert!(
            ready.contains("blocking: configuration persistence outcome unknown"),
            "{ready}"
        );
        assert!(!ready.contains(TOKEN));
        let live = probe(app, "/livez").await;
        assert!(
            live.starts_with("HTTP/1.1 200") && live.ends_with("alive\n"),
            "{live}"
        );
        assert!(
            response.contains(
                "configuration may be applied; persistence outcome unknown; inspect before retrying"
            )
        );
        assert!(!response.contains("\r\netag:"));
        assert!(!response.contains("\r\nretry-after:"));
        let published = app.store.status().version();
        assert_ne!(&published, committed.version());
        assert_eq!(
            app.activated.load(Ordering::Acquire),
            published.generation.number()
        );
        assert_serving(app).await;
        assert_eq!(std::fs::read(&backup).unwrap(), saved);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        // Fixing the pathname does not allow an HTTP retry to clear the latch.
        let response = put(server.address(), &published.etag(), &body).await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert_eq!(app.store.status().version(), published);
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        server.shutdown().await.unwrap();
        app.shutdown().await;
        drop(api);
        drop(fixture.writer);
        let (_reader, snapshots) =
            CheckpointWriter::open(&fixture.directory, "fixture", &app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(
            gatel_model::snapshot_codec::encode(&serving).unwrap(),
            gatel_model::snapshot_codec::encode(committed.committed().unwrap().source_snapshot()).unwrap()
        );
        assert_eq!(
            gatel_model::snapshot_codec::encode(&observed).unwrap(),
            gatel_model::snapshot_codec::encode(committed.observed_snapshot()).unwrap()
        );
        let restored = Arc::new(
            App::with_sources(std::env::temp_dir(), [SourceId::file("main"), SourceId::admin()]).unwrap(),
        );
        let reader = Arc::new(Mutex::new(_reader));
        assert!(matches!(
            restored
                .recover_checkpoint_if(
                    restored.store.status().version(),
                    serving,
                    observed,
                    reader.clone()
                )
                .await
                .unwrap(),
            Outcome::Started { .. }
        ));
        let ready = probe(&restored, "/readyz").await;
        assert!(
            ready.starts_with("HTTP/1.1 200") && ready.ends_with("ready\n"),
            "{ready}"
        );
        assert_ne!(
            restored.configuration_status().boot_id,
            app.configuration_status().boot_id
        );
        restored.shutdown().await;
    })
    .await
    .unwrap();
}
