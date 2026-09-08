use std::future::Future;
use std::task::{Context, Waker};
use std::time::Duration;

use gatel_model::{Hasher, SourceRev};

use super::*;

fn source(id: SourceId, revision: u64, text: &str) -> SourceSnapshot {
    let mut hash = Hasher::new();
    hash.update_str(text);
    SourceSnapshot::new(
        id,
        SourceRev(revision),
        hash.finish(),
        gatel_kdl::lower(text, "recovery-fixture").unwrap(),
    )
}

fn fixture() -> (Arc<App>, Snapshot, Snapshot, std::net::SocketAddr) {
    let app = Arc::new(
        App::with_sources(std::env::temp_dir(), [SourceId::file("main"), SourceId::admin()]).unwrap(),
    );
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    app.binds.adopt(socket).unwrap();
    let text = format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"recovered-live\"; }} }}"
    );
    let serving = Snapshot::new().with(source(SourceId::file("main"), 1, &text));
    let observed = serving.clone().with(source(
        SourceId::admin(),
        4,
        "invalid-directive \"secret-rejected\";",
    ));
    (app, serving, observed, address)
}

#[tokio::test]
async fn file_reload_after_recovery_advances_from_recovered_revisions() {
    tokio::time::timeout(Duration::from_secs(10), async {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for multiple in [false, true] {
            let directory = Directory(std::env::temp_dir().join(format!(
                "gatel-recovered-files-{}",
                gatel_model::BootId::generate()
            )));
            std::fs::create_dir(&directory.0).unwrap();
            let root = directory.0.join("main.kdl");
            let extra = directory.0.join("extra.kdl");
            std::fs::write(&extra, "").unwrap();
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = socket.local_addr().unwrap();
            let config = |body| {
                format!(
                    "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}"
                )
            };
            std::fs::write(&root, config("old")).unwrap();
            let mut main = gatel_kdl::snapshot(&root, SourceId::file("main"), None).unwrap();
            main.rev = SourceRev(40);
            let mut serving = Snapshot::new().with(main);
            let mut extras = vec![];
            if multiple {
                let mut part = gatel_kdl::snapshot(&extra, SourceId::file("extra"), None).unwrap();
                part.rev = SourceRev(70);
                serving.insert(part);
                extras.push(format!("extra={}", extra.display()).parse().unwrap());
            }
            let observed = serving
                .clone()
                .with(source(SourceId::file("main"), 41, "invalid-directive;"));
            let app = Arc::new(App::new_with_files(root.clone(), extras).unwrap());
            app.binds.adopt(socket).unwrap();
            assert!(matches!(
                app.recover_configuration(app.configuration_status().version(), serving, observed)
                    .await,
                Ok(Outcome::Started { .. })
            ));
            assert!(matches!(app.reload().await, Outcome::NoOp { .. }));
            assert_eq!(
                app.store
                    .observed_snapshot()
                    .1
                    .get(&SourceId::file("main"))
                    .unwrap()
                    .rev,
                SourceRev(42)
            );
            std::fs::write(&root, config("new")).unwrap();
            assert!(matches!(app.reload().await, Outcome::Reloaded { .. }));
            let current = app.store.observed_snapshot().1;
            assert_eq!(current.get(&SourceId::file("main")).unwrap().rev, SourceRev(43));
            if multiple {
                assert_eq!(current.get(&SourceId::file("extra")).unwrap().rev, SourceRev(70));
            }
            assert_eq!(app.server.active_addresses().await, vec![address]);
            app.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn recovery_activates_real_http_while_preserving_rejected_intent() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (app, serving, observed, address) = fixture();
        let version = app.configuration_status().version();
        assert!(matches!(
            app.recover_configuration(version.clone(), serving.clone(), observed.clone())
                .await,
            Ok(Outcome::Started { .. })
        ));
        assert_eq!(app.server.active_addresses().await, vec![address]);
        assert_eq!(app.store.observed_snapshot().1.revs(), observed.revs());
        assert_eq!(app.store.current().unwrap().revs, serving.revs());
        assert_eq!(app.activated.load(std::sync::atomic::Ordering::Acquire), 1);
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("recovered-live"));
        assert!(matches!(
            app.recover_configuration(app.configuration_status().version(), serving, observed)
                .await,
            Err(gatel_plan::RestoreIntentError::AlreadyPublished)
        ));
        app.shutdown().await;
        assert!(app.server.active_addresses().await.is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_recovery_pair_never_publishes_or_activates() {
    let (app, serving, ..) = fixture();
    let expected = app.configuration_status().version();
    assert!(matches!(
        app.recover_configuration(expected.clone(), serving, Snapshot::new())
            .await,
        Err(gatel_plan::RestoreIntentError::InvalidSources)
    ));
    assert_eq!(app.configuration_status().version(), expected);
    assert!(app.store.current().is_none());
    assert!(app.server.active_addresses().await.is_empty());
    app.shutdown().await;
}

#[tokio::test]
async fn accepted_recovery_survives_waiter_cancellation_through_activation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (app, serving, observed, address) = fixture();
        let activation = app.lifecycle.lock().await;
        let mut waiter = Box::pin(app.recover_configuration(
            app.configuration_status().version(),
            serving,
            observed.clone(),
        ));
        assert!(
            waiter
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(waiter);
        while app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        assert!(app.server.active_addresses().await.is_empty());
        assert_eq!(app.store.observed_snapshot().1.revs(), observed.revs());
        assert!(app.reload_execution.try_lock().is_err());
        drop(activation);
        app.submissions.drained().await;
        assert_eq!(app.server.active_addresses().await, vec![address]);
        assert_eq!(
            app.reload_slots.available_permits(),
            super::super::MAX_RELOAD_TASKS
        );
        app.shutdown().await;
    })
    .await
    .unwrap();
}
