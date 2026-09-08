use gatel_model::{Hasher, SourceRev};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn admin(snapshot: &mut Snapshot, text: &str) {
    let source = SourceId::admin();
    let previous = snapshot.get(&source).map(|source| source.rev);
    let mut hash = Hasher::new();
    hash.update_str(text);
    snapshot
        .observe_if(
            source,
            previous,
            hash.finish(),
            gatel_kdl::lower(text, "admin-fixture").unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn late_activation_does_not_clear_a_newer_rejected_intent() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let source = SourceId::file("main");
        let nodes = gatel_kdl::lower(
            &format!(
                "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"healthy\"; }} }}"
            ),
            "health",
        )
        .unwrap();
        let initial = Snapshot::new().with(SourceSnapshot::new(
            source.clone(),
            SourceRev(1),
            Hasher::new().finish(),
            nodes,
        ));
        let app = Arc::new(App::from_snapshot(std::env::temp_dir(), initial).unwrap());
        app.binds.adopt(socket).unwrap();
        let activation = app.lifecycle.lock().await;
        let caller = {
            let app = app.clone();
            tokio::spawn(async move { app.reload().await })
        };
        while app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        let (version, mut intent) = app.store.observed_snapshot();
        let mut hash = Hasher::new();
        hash.update_str("invalid");
        intent.insert(SourceSnapshot::new(
            source,
            SourceRev(2),
            hash.finish(),
            gatel_kdl::lower("nonsense;", "health").unwrap(),
        ));
        assert!(matches!(
            app.attempt_snapshot_if(&version, &intent),
            Ok(Outcome::Rejected(_))
        ));
        let summary = app.store.status().last_failure.unwrap().summary;
        drop(activation);
        assert!(matches!(caller.await.unwrap(), Outcome::Started { .. }));
        assert!(app.telemetry.health.report().render().contains(&summary));
        assert!(!app.server.active_addresses().await.is_empty());
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn noop_still_activates_a_generation_published_by_a_low_level_caller() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let directory = Directory(
            std::env::temp_dir().join(format!("gatel-noop-activate-{}", gatel_model::BootId::generate())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let root = directory.0.join("main.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        std::fs::write(
            &root,
            format!(
                "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"activated\"; }} }}"
            ),
        )
        .unwrap();
        let app = Arc::new(App::new(root).unwrap());
        app.binds.adopt(socket).unwrap();
        assert!(matches!(app.attempt(), Outcome::Started { .. }));
        let committed = app.store.current().unwrap();
        assert!(app.server.active_addresses().await.is_empty());
        assert_eq!(app.activated.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(matches!(app.reload().await, Outcome::NoOp { .. }));
        assert_eq!(
            app.activated.load(std::sync::atomic::Ordering::Acquire),
            committed.number.number()
        );
        assert!(Arc::ptr_eq(&committed, &app.store.current().unwrap()));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("activated"));
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn restoring_identical_file_clears_reload_failure_without_replacing_runtime() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let directory = Directory(
            std::env::temp_dir().join(format!("gatel-noop-recovery-{}", gatel_model::BootId::generate())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let root = directory.0.join("main.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let config = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"unchanged\"; }} }}"
        );
        std::fs::write(&root, &config).unwrap();
        let app = Arc::new(App::new(root.clone()).unwrap());
        app.binds.adopt(socket).unwrap();
        assert!(matches!(app.reload().await, Outcome::Started { .. }));
        let serving = app.store.current().unwrap();
        for broken in ["site {", "not-a-supported-root-node;"] {
            std::fs::write(&root, broken).unwrap();
            assert!(matches!(app.reload().await, Outcome::Rejected(_)));
            assert!(app.telemetry.health.report().render().contains("degraded:"));
            std::fs::write(&root, &config).unwrap();
            // Recovery for an already active generation must not queue a
            // redundant full activation behind this lifecycle lock.
            let activation = app.lifecycle.lock().await;
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), app.reload())
                .await
                .expect("active NoOp unnecessarily waited for lifecycle coordination");
            assert!(matches!(outcome, Outcome::NoOp { .. }));
            drop(activation);
            assert!(Arc::ptr_eq(&serving, &app.store.current().unwrap()));
            assert!(!app.telemetry.health.report().render().contains("degraded:"));
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            assert!(response.ends_with("unchanged"));
        }
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn file_reload_preserves_runtime_intent_including_rejection_and_tombstones() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let directory = Directory(
            std::env::temp_dir().join(format!("gatel-source-union-{}", gatel_model::BootId::generate())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let root = directory.0.join("main.kdl");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let config = |body: &str| {
            format!(
                "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}"
            )
        };
        std::fs::write(&root, config("first")).unwrap();
        // Trusted host registration only: there is no network Admin endpoint.
        let app =
            Arc::new(App::with_sources(root.clone(), [SourceId::file("main"), SourceId::admin()]).unwrap());
        app.binds.adopt(socket).unwrap();
        assert!(matches!(app.reload().await, Outcome::Started { .. }));
        let request = || async {
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            response
        };
        let (version, mut intent) = app.store.observed_snapshot();
        admin(
            &mut intent,
            "upstream \"runtime/shared\" { endpoint \"127.0.0.1:1\"; }",
        );
        let submitted = intent.get(&SourceId::admin()).unwrap().clone();
        assert!(matches!(
            app.reload_source_if(version.clone(), submitted.clone()).await,
            Ok(Outcome::Reloaded { .. })
        ));
        let committed = app.store.current().unwrap();
        let current = app.store.status().version();
        assert_eq!(
            app.reload_source_if(version, submitted).await.unwrap_err().actual,
            current
        );
        assert!(Arc::ptr_eq(&committed, &app.store.current().unwrap()));
        assert_eq!(app.store.current().unwrap().plan.upstreams.len(), 1);
        std::fs::write(&root, config("second")).unwrap();
        assert!(matches!(app.reload().await, Outcome::Reloaded { .. }));
        assert_eq!(app.store.current().unwrap().plan.upstreams.len(), 1);
        assert!(request().await.ends_with("second"));

        let (version, mut intent) = app.store.observed_snapshot();
        admin(&mut intent, "not-a-supported-root-node;");
        assert!(matches!(
            app.reload_source_if(version, intent.get(&SourceId::admin()).unwrap().clone())
                .await,
            Ok(Outcome::Rejected(_))
        ));
        let serving = app.store.current().unwrap();
        std::fs::write(&root, config("third")).unwrap();
        assert!(matches!(app.reload().await, Outcome::Rejected(_)));
        assert!(Arc::ptr_eq(&serving, &app.store.current().unwrap()));
        assert!(request().await.ends_with("second"));
        let (version, mut intent) = app.store.observed_snapshot();
        assert_eq!(intent.get(&SourceId::file("main")).unwrap().rev, SourceRev(3));
        assert_eq!(intent.get(&SourceId::admin()).unwrap().rev, SourceRev(2));
        admin(&mut intent, "");
        let tombstone = intent.get(&SourceId::admin()).unwrap().clone();
        assert!(matches!(
            app.reload_source_if(version, tombstone.clone()).await,
            Ok(Outcome::Reloaded { .. })
        ));
        assert!(request().await.ends_with("third"));
        assert!(app.store.current().unwrap().plan.upstreams.is_empty());
        assert!(matches!(app.reload().await, Outcome::NoOp { .. }));
        let (_, intent) = app.store.observed_snapshot();
        let removed = intent.get(&SourceId::admin()).unwrap();
        assert_eq!(removed.rev, SourceRev(3));
        assert!(removed.nodes.is_empty());
        app.shutdown().await;
        assert!(app.binds.bound().is_empty());
        let sealed = app.store.status().version();
        assert!(matches!(
            app.reload_source_if(sealed.clone(), tombstone).await,
            Ok(Outcome::Stale(_))
        ));
        assert_eq!(app.store.status().version(), sealed);
    })
    .await
    .unwrap();
}
