use gatel_model::{Hasher, SourceRev};

use super::*;

#[path = "app_test_phase.rs"]
mod phase;

#[tokio::test]
async fn named_file_sources_publish_reload_and_keep_lkg_on_failed_observation() {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _total = phase::Phase::new("total");
    let setup = phase::Phase::new("setup");
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            phase::run("fixture.cleanup", || {
                let _ = std::fs::remove_dir_all(&self.0);
            });
        }
    }
    let directory =
        Directory(std::env::temp_dir().join(format!("gatel-multi-file-{}", gatel_model::BootId::generate())));
    std::fs::create_dir(&directory.0).unwrap();
    let root = directory.0.join("gatel.kdl");
    let extra = directory.0.join("routes.kdl");
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    std::fs::write(&root, format!("global {{ http \"{address}\"; }}")).unwrap();
    let config = |body: &str| format!("site \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}");
    std::fs::write(&extra, config("first")).unwrap();
    let app =
        App::new_with_files(root, vec![format!("routes={}", extra.display()).parse().unwrap()]).unwrap();
    app.binds.adopt(socket).unwrap();
    drop(setup);
    let request = |label| async move {
        phase::wait(label, Duration::from_secs(3), async {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8(bytes).unwrap()
        })
        .await
    };
    assert!(matches!(
        phase::run("initial.attempt", || app.attempt()),
        Outcome::Started { .. }
    ));
    phase::wait("initial.activate", Duration::from_secs(20), app.activate()).await;
    assert!(request("initial.http").await.ends_with("first"));
    phase::run("invalid.write", || std::fs::write(&extra, "site {\n").unwrap());
    assert!(matches!(
        phase::run("invalid.attempt", || app.attempt()),
        Outcome::Rejected(_)
    ));
    assert!(request("invalid.http").await.ends_with("first"));
    phase::run("recovery.write", || {
        std::fs::write(&extra, config("second")).unwrap()
    });
    assert!(matches!(
        phase::run("recovery.attempt", || app.attempt()),
        Outcome::Reloaded { .. }
    ));
    phase::wait("recovery.activate", Duration::from_secs(20), app.activate()).await;
    assert!(request("recovery.http").await.ends_with("second"));
    assert_eq!(
        app.store.current().unwrap().revs.get(&SourceId::file("main")),
        Some(SourceRev(1))
    );
    assert_eq!(
        app.store.current().unwrap().revs.get(&SourceId::file("routes")),
        Some(SourceRev(2))
    );
    assert!(matches!(
        phase::run("noop.attempt", || app.attempt()),
        Outcome::NoOp { .. }
    ));
    // Exercise the real watcher loop, including its polling fallback. Do not
    // require a particular OS notification delivery order or timing.
    let app = Arc::new(app);
    let watcher = tokio::spawn(crate::watch::reload_on_change(app.clone()));
    struct StopWatcher(tokio::task::AbortHandle);
    impl Drop for StopWatcher {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop_watcher = StopWatcher(watcher.abort_handle());
    phase::run("watch.write", || {
        std::fs::write(&extra, config("watched")).unwrap()
    });
    phase::wait("watch.reload", Duration::from_secs(12), async {
        loop {
            if request("watch.http").await.ends_with("watched") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    // A new child directory, not subscribed when the watcher started.
    let external = directory.0.join("imported");
    let import_setup = phase::Phase::new("import.setup");
    std::fs::create_dir(&external).unwrap();
    let imported = external.join("routes.kdl");
    std::fs::write(&imported, config("external-first")).unwrap();
    std::fs::write(&extra, "import \"imported/routes.kdl\"\n").unwrap();
    drop(import_setup);
    for body in ["external-first", "external-second"] {
        if body == "external-second" {
            phase::run("import.write", || {
                std::fs::write(&imported, config(body)).unwrap()
            });
        }
        phase::wait(body, Duration::from_secs(12), async {
            while !request("import.http").await.ends_with(body) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
    }
    watcher.abort();
    assert!(
        phase::wait("watcher.join", Duration::from_secs(20), watcher)
            .await
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(
        app.store.current().unwrap().revs.get(&SourceId::file("routes")),
        Some(SourceRev(5))
    );
    phase::wait("app.shutdown", Duration::from_secs(20), app.shutdown()).await;
}

#[test]
fn file_app_does_not_admit_unregistered_runtime_sources() {
    let app = App::new(PathBuf::from("registration-fixture-not-read")).unwrap();
    let snapshot = Snapshot::new().with(SourceSnapshot::new(
        SourceId::admin(),
        SourceRev(1),
        Hasher::new().finish(),
        vec![],
    ));
    let outcome = app
        .store
        .attempt(&snapshot, &app.registry, |_| panic!("unregistered source"));
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert!(app.store.status().revs.is_empty());
}

#[test]
fn source_read_and_cache_update_share_one_lock_and_failure_retains_previous() {
    let app = App::new(PathBuf::from("observation-fixture-not-read")).unwrap();
    for revision in 1..=2 {
        let result = app
            .observe_source(|previous| {
                // The entire source read/parse/revision assignment runs under the
                // observation lock; a second loader cannot read the old cursor.
                assert!(matches!(
                    app.previous.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                assert_eq!(
                    previous.map(|source| source.rev),
                    if revision == 1 {
                        None
                    } else {
                        Some(SourceRev(revision - 1))
                    }
                );
                let mut content = Hasher::new();
                content.update_u64(revision);
                Ok(SourceSnapshot::new(
                    app.source.clone(),
                    SourceRev(revision),
                    content.finish(),
                    vec![],
                ))
            })
            .unwrap();
        assert_eq!(result.get(&app.source).unwrap().rev, SourceRev(revision));
    }
    assert!(
        app.observe_source(|previous| {
            assert_eq!(previous.unwrap().rev, SourceRev(2));
            Err(Diagnostics::new())
        })
        .is_err()
    );
    assert_eq!(app.previous.lock().unwrap().as_ref().unwrap().rev, SourceRev(2));
}
