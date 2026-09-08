use super::*;

#[tokio::test]
async fn initial_observation_failures_allow_corrected_input_without_reopening_writer() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let root = fixture.directory.join("initial.kdl");
        let app = Arc::new(App::new_with_admin(root.clone(), vec![]).unwrap());
        let source = app
            .checkpoint_source_writer(SourceId::admin(), fixture.writer.clone())
            .unwrap();
        let initial = app.store.status().version();
        let log = std::fs::read(fixture.directory.join("intent.log")).unwrap();
        assert!(matches!(
            source.reload_kdl_if(initial.clone(), "site {".into()).await,
            Ok(Outcome::Rejected(_))
        ));
        // First the configured root is absent, then it exists but cannot parse.
        for malformed in [false, true] {
            if malformed {
                std::fs::write(&root, "site {").unwrap();
            }
            assert!(matches!(
                app.reload_frontend_checkpoint_if(initial.clone(), fixture.writer.clone())
                    .await,
                Ok(Outcome::Rejected(_))
            ));
            assert_eq!(app.store.status().version(), initial);
            assert!(app.store.current().is_none());
            assert!(app.store.observed_snapshot().1.sources().next().is_none());
            assert_eq!(app.store.status().last_failure.unwrap().stage, "observe");
            fixture.writer.lock().unwrap().ensure_ready().unwrap();
            assert!(!fixture.directory.join("checkpoint.bin").exists());
            assert_eq!(std::fs::read(fixture.directory.join("intent.log")).unwrap(), log);
        }
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        app.binds.adopt(socket).unwrap();
        std::fs::write(
            &root,
            format!(
                "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint\"; }} }}"
            ),
        )
        .unwrap();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(initial, fixture.writer.clone())
                .await,
            Ok(Outcome::Started { .. })
        ));
        assert!(fixture.directory.join("checkpoint.bin").is_file());
        assert!(app.store.status().last_failure.is_none());
        assert_serving(&app).await;
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn checkpointed_file_reload_preserves_admin_intent_and_fences_observation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let root = fixture.directory.join("main.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let config = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"checkpoint\"; }} }}"
        );
        std::fs::write(&root, &config).unwrap();
        let app = Arc::new(App::new_with_admin(root.clone(), vec![]).unwrap());
        app.binds.adopt(socket).unwrap();
        let initial = app.store.status().version();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(initial.clone(), fixture.writer.clone())
                .await,
            Ok(Outcome::Started { .. })
        ));
        let source = app
            .checkpoint_source_writer(SourceId::admin(), fixture.writer.clone())
            .unwrap();
        assert!(matches!(
            source
                .reload_kdl_if(
                    app.store.status().version(),
                    "upstream \"runtime/retained\" { endpoint \"127.0.0.1:2\"; }".into()
                )
                .await,
            Ok(Outcome::Reloaded { .. })
        ));
        let path = fixture.directory.join("checkpoint.bin");
        let saved = std::fs::read(&path).unwrap();
        // A stale caller must not even observe this otherwise valid edit.
        std::fs::write(&root, format!("{config}\n// second input\n")).unwrap();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(initial, fixture.writer.clone())
                .await,
            Err(SourceWriteError::Conflict(_))
        ));
        // If the stale call advanced the file cache, restoring the original
        // bytes would allocate revision 3 instead of retaining revision 1.
        std::fs::write(&root, &config).unwrap();
        assert_eq!(
            app.files
                .as_ref()
                .unwrap()
                .observe()
                .unwrap()
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(1)
        );
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        std::fs::write(&root, format!("{config}\n// second input\n")).unwrap();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(app.store.status().version(), fixture.writer.clone())
                .await,
            Ok(Outcome::NoOp { .. })
        ));
        assert_eq!(
            app.store
                .observed_snapshot()
                .1
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(2)
        );
        assert_eq!(app.store.current().unwrap().plan.upstreams.len(), 1);
        // A validly parsed but rejected Admin intent must not disappear when
        // the file frontend next refreshes its contribution.
        assert!(matches!(
            source
                .reload_kdl_if(app.store.status().version(), "unsupported-root;".into())
                .await,
            Ok(Outcome::Rejected(_))
        ));
        std::fs::write(&root, format!("{config}\n// third input\n")).unwrap();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(app.store.status().version(), fixture.writer.clone())
                .await,
            Ok(Outcome::Rejected(_))
        ));
        let capture = app.store.capture_configuration();
        assert_eq!(
            capture.observed_snapshot().get(&SourceId::admin()).unwrap().rev,
            SourceRev(2)
        );
        assert_eq!(
            capture
                .observed_snapshot()
                .get(&SourceId::file("main"))
                .unwrap()
                .rev,
            SourceRev(3)
        );
        assert_serving(&app).await;
        let saved = std::fs::read(&path).unwrap();
        let version = app.store.status().version();
        fixture.writer.lock().unwrap().require_reconciliation();
        std::fs::write(&root, "site {").unwrap();
        assert!(matches!(
            app.reload_frontend_checkpoint_if(version.clone(), fixture.writer.clone())
                .await,
            Err(SourceWriteError::PersistenceUnknown)
        ));
        assert_eq!(app.store.status().version(), version);
        assert_eq!(app.store.status().last_failure.unwrap().stage, "normalize");
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        app.shutdown().await;
        drop(source);
        drop(fixture.writer);
        let (_reader, snapshots) =
            CheckpointWriter::open(&fixture.directory, "fixture", &app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(
            gatel_model::snapshot_codec::encode(&serving).unwrap(),
            gatel_model::snapshot_codec::encode(capture.committed().unwrap().source_snapshot()).unwrap()
        );
        assert_eq!(
            gatel_model::snapshot_codec::encode(&observed).unwrap(),
            gatel_model::snapshot_codec::encode(capture.observed_snapshot()).unwrap()
        );
    })
    .await
    .unwrap();
}
