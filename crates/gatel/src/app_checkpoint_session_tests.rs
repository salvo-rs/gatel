use super::*;
use crate::admin::server::AdminServer;
use crate::app::CheckpointSession;

#[tokio::test]
async fn checkpoint_watcher_saves_file_changes_and_returns_persistence_failure() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let fixture = Fixture::new();
        drop(fixture.writer);
        let root = fixture.directory.join("watched.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let config = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint\"; }} }}"
        );
        std::fs::write(&root, &config).unwrap();
        let app = App::new_with_admin(root.clone(), vec![]).unwrap();
        app.binds.adopt(socket).unwrap();
        let mut session = CheckpointSession::open(app, &fixture.directory, "fixture").unwrap();
        assert!(matches!(
            session.startup().await.unwrap(),
            Outcome::Started { .. }
        ));
        let session = Arc::new(session);
        let watching = session.clone();
        let watcher = tokio::spawn(async move {
            crate::watch::reload_checkpoint_until(watching, std::future::pending()).await
        });
        let path = fixture.directory.join("checkpoint.bin");
        let before = std::fs::read(&path).unwrap();
        // A startup notification may be missed; the real fallback poll must
        // still observe this change. No timer advancement or fake event sender.
        std::fs::write(&root, format!("{config}\n// second\n")).unwrap();
        while session
            .app()
            .store
            .observed_snapshot()
            .1
            .get(&SourceId::file("main"))
            .unwrap()
            .rev
            != SourceRev(2)
        {
            tokio::task::yield_now().await;
        }
        session.app().submissions.drained().await;
        let saved = std::fs::read(&path).unwrap();
        assert_ne!(saved, before);
        assert_serving(session.app()).await;
        let backup = fixture.directory.join("watch-confirmed.bin");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(&root, format!("{config}\n// third\n")).unwrap();
        assert!(matches!(
            watcher.await.unwrap(),
            Err(SourceWriteError::PersistenceUnknown)
        ));
        assert_eq!(
            session
                .app()
                .store
                .observed_snapshot()
                .1
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(3)
        );
        assert_eq!(std::fs::read(&backup).unwrap(), saved);
        // The watcher returns to the host; it neither stops the serving plane
        // itself nor falls back to an uncheckpointed write path.
        assert_serving(session.app()).await;
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        assert!(matches!(
            session.reload().await,
            Err(SourceWriteError::PersistenceUnknown)
        ));
        session.app().shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_coordinates_startup_http_file_reload_and_restart() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        drop(fixture.writer);
        let root = fixture.directory.join("session.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let config = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint\"; }} }}"
        );
        std::fs::write(&root, &config).unwrap();
        let app = App::new_with_admin(root.clone(), vec![]).unwrap();
        app.binds.adopt(socket).unwrap();
        let token = || zeroize::Zeroizing::new("0123456789abcdef0123456789abcdef".into());
        let mut session = CheckpointSession::open(app, &fixture.directory, "fixture").unwrap();
        assert!(session.admin_api(token()).is_err());
        assert!(matches!(
            session.reload().await,
            Err(SourceWriteError::Unavailable)
        ));
        assert!(matches!(
            session.startup().await.unwrap(),
            Outcome::Started { .. }
        ));
        assert!(session.startup().await.is_err());
        let api = Arc::new(session.admin_api(token()).unwrap());
        let server =
            AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), api.clone()).unwrap();
        let response = super::http_tests::put(
            server.address(),
            &session.app().configuration_status().version().etag(),
            "upstream \"runtime/session\" { endpoint \"127.0.0.1:2\"; }",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        std::fs::write(&root, format!("{config}\n// second\n")).unwrap();
        assert!(matches!(session.reload().await.unwrap(), Outcome::NoOp { .. }));
        let capture = session.app().store.capture_configuration();
        let boot = capture.version().boot_id;
        assert_eq!(
            capture
                .observed_snapshot()
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(2)
        );
        assert_eq!(
            capture.observed_snapshot().get(&SourceId::admin()).unwrap().rev,
            SourceRev(1)
        );
        assert_serving(session.app()).await;
        server.shutdown().await.unwrap();
        session.app().shutdown().await;
        drop(api);
        drop(session);
        // Existing checkpoints bootstrap independently of newly broken files.
        std::fs::write(&root, "site {").unwrap();
        let mut recovered = CheckpointSession::open(
            App::new_with_admin(root.clone(), vec![]).unwrap(),
            &fixture.directory,
            "fixture",
        )
        .unwrap();
        assert!(recovered.admin_api(token()).is_err());
        assert!(matches!(
            recovered.startup().await.unwrap(),
            Outcome::Started { .. }
        ));
        assert_ne!(recovered.app().configuration_status().boot_id, boot);
        let restored = recovered.app().store.capture_configuration();
        assert_eq!(
            gatel_model::snapshot_codec::encode(restored.committed().unwrap().source_snapshot()).unwrap(),
            gatel_model::snapshot_codec::encode(capture.committed().unwrap().source_snapshot()).unwrap()
        );
        assert_eq!(
            gatel_model::snapshot_codec::encode(restored.observed_snapshot()).unwrap(),
            gatel_model::snapshot_codec::encode(capture.observed_snapshot()).unwrap()
        );
        assert_serving(recovered.app()).await;
        assert!(matches!(recovered.reload().await.unwrap(), Outcome::Rejected(_)));
        std::fs::write(&root, format!("{config}\n// third\n")).unwrap();
        assert!(matches!(recovered.reload().await.unwrap(), Outcome::NoOp { .. }));
        assert_eq!(
            recovered
                .app()
                .store
                .observed_snapshot()
                .1
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(3)
        );
        assert_eq!(recovered.app().store.current().unwrap().plan.upstreams.len(), 1);
        recovered.app().shutdown().await;
    })
    .await
    .unwrap();
}
