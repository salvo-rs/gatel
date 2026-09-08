use super::*;

#[tokio::test]
async fn cancelled_source_waiter_keeps_checkpoint_owned_through_shutdown() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::with_sources([SourceId::file("main"), SourceId::admin()]);
        let app = &fixture.app;
        assert!(matches!(
            app.reload_checkpoint_if(
                app.store.status().version(),
                fixture.snapshot.clone(),
                fixture.writer.clone(),
            )
            .await,
            Ok(Outcome::Started { .. })
        ));
        let path = fixture.directory.join("checkpoint.bin");
        let before = std::fs::read(&path).unwrap();
        let source = app
            .checkpoint_source_writer(SourceId::admin(), fixture.writer.clone())
            .unwrap();
        let version = app.store.status().version();
        let lifecycle = app.lifecycle.lock().await;
        let caller = tokio::spawn(async move {
            source
                .reload_kdl_if(
                    version,
                    "upstream \"runtime/cancelled\" { endpoint \"127.0.0.1:2\"; }".into(),
                )
                .await
        });
        while app.store.observed_snapshot().1.get(&SourceId::admin()).is_none() {
            tokio::task::yield_now().await;
        }
        // Wait for publication, not just observation: activation is fenced by
        // lifecycle, so completion cannot have taken the checkpoint lock yet.
        while app
            .store
            .current()
            .unwrap()
            .source_snapshot()
            .get(&SourceId::admin())
            .is_none()
        {
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
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(lifecycle);
        let published = app.store.current().unwrap().number.number();
        while app.activated.load(Ordering::Acquire) < published {
            tokio::task::yield_now().await;
        }
        assert_serving(app).await;
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(app.reload_execution.try_lock().is_err());
        let closing = app.clone();
        let shutdown = tokio::spawn(async move { closing.shutdown().await });
        while !app.store.is_sealed() {
            tokio::task::yield_now().await;
        }
        assert!(!shutdown.is_finished());
        assert!(app.source_writer(SourceId::admin()).is_none());
        release.send(()).unwrap();
        obstruction.await.unwrap();
        shutdown.await.unwrap();
        assert_ne!(std::fs::read(&path).unwrap(), before);
        assert!(app.server.active_addresses().await.is_empty());
        assert_eq!(app.reload_slots.available_permits(), crate::app::MAX_RELOAD_TASKS);
        let capture = app.store.capture_configuration();
        fixture.writer.lock().unwrap().ensure_ready().unwrap();
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

#[tokio::test]
async fn bound_source_checkpoints_union_rejection_and_failure_latch() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::with_sources([SourceId::file("main"), SourceId::admin()]);
        let app = &fixture.app;
        assert!(
            app.checkpoint_source_writer(SourceId::file("unknown"), fixture.writer.clone())
                .is_none()
        );
        assert!(matches!(
            app.reload_checkpoint_if(
                app.store.status().version(),
                fixture.snapshot.clone(),
                fixture.writer.clone(),
            )
            .await,
            Ok(Outcome::Started { .. })
        ));
        let source = app
            .checkpoint_source_writer(SourceId::admin(), fixture.writer.clone())
            .unwrap();
        let initial = app.store.status().version();
        let valid = "upstream \"runtime/inline\" { endpoint \"127.0.0.1:2\"; }";
        assert!(matches!(
            source.reload_kdl_if(initial.clone(), valid.into()).await,
            Ok(Outcome::Reloaded { .. })
        ));
        let path = fixture.directory.join("checkpoint.bin");
        let saved = std::fs::read(&path).unwrap();
        let published = app.store.status().version();
        // CAS precedes parsing, writer ownership, observation and disk changes.
        assert!(matches!(
            source.reload_kdl_if(initial, "site {".into()).await,
            Err(SourceWriteError::Conflict(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert_eq!(app.store.status().version(), published);
        assert!(app.store.status().last_failure.is_none());
        assert!(matches!(
            source.reload_kdl_if(published.clone(), "site {".into()).await,
            Ok(Outcome::Rejected(_))
        ));
        assert_eq!(app.store.status().version(), published);
        // Parse failures are diagnostics, not a new serializable source intent.
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert!(matches!(
            source
                .reload_kdl_if(published.clone(), "unsupported-root;".into())
                .await,
            Ok(Outcome::Rejected(_))
        ));
        let capture = app.store.capture_configuration();
        assert_eq!(
            capture
                .committed()
                .unwrap()
                .source_snapshot()
                .get(&SourceId::admin())
                .unwrap()
                .rev,
            SourceRev(1)
        );
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
            SourceRev(1)
        );
        assert_serving(app).await;
        let saved = std::fs::read(&path).unwrap();
        fixture.writer.lock().unwrap().require_reconciliation();
        let version = app.store.status().version();
        assert!(matches!(
            source.reload_kdl_if(version.clone(), valid.into()).await,
            Err(SourceWriteError::PersistenceUnknown)
        ));
        assert_eq!(app.store.status().version(), version);
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        app.shutdown().await;
        drop(source);
        drop(fixture.writer);
        let (reopened, recovered) =
            CheckpointWriter::open(&fixture.directory, "fixture", &app.registry).unwrap();
        let (serving, observed) = recovered.unwrap();
        assert_eq!(
            gatel_model::snapshot_codec::encode(&serving).unwrap(),
            gatel_model::snapshot_codec::encode(capture.committed().unwrap().source_snapshot()).unwrap()
        );
        assert_eq!(
            gatel_model::snapshot_codec::encode(&observed).unwrap(),
            gatel_model::snapshot_codec::encode(capture.observed_snapshot()).unwrap()
        );
        assert!(reopened.ensure_ready().is_err());
    })
    .await
    .unwrap();
}
