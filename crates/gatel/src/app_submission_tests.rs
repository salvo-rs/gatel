use gatel_model::Hasher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[tokio::test]
async fn cancelled_callers_cannot_bypass_the_total_reload_task_limit() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let app = Arc::new(App::new(PathBuf::from("reload-capacity-unread.kdl")).unwrap());
    let execution = app.configuration_slot.clone().acquire_owned().await.unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..MAX_RELOAD_TASKS {
        let mut caller = Box::pin(app.reload());
        assert!(caller.as_mut().poll(&mut cx).is_pending());
        // Accepted tasks remain owned and counted after their waiter is gone.
        drop(caller);
    }
    assert_eq!(app.reload_slots.available_permits(), 0);
    let before = app.store.status().version();
    let overflow = app.reload().await;
    assert!(matches!(overflow, Outcome::Stale(ref reason) if reason.contains("capacity exhausted")));
    assert_eq!(app.store.status().version(), before);
    assert!(app.previous.lock().unwrap().is_none());
    assert!(matches!(
        app.reload_snapshot_if(before.clone(), Snapshot::new()).await,
        Ok(Outcome::Stale(ref reason)) if reason.contains("capacity exhausted")
    ));
    assert!(matches!(
        app.reload_source_if(
            before.clone(),
            SourceSnapshot::new(SourceId::file("main"), gatel_model::SourceRev(1), Hasher::new().finish(), vec![]),
        ).await,
        Ok(Outcome::Stale(ref reason)) if reason.contains("capacity exhausted")
    ));
    drop(execution);
    tokio::time::timeout(std::time::Duration::from_secs(5), app.submissions.drained())
        .await
        .unwrap();
    assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
    // The deliberately missing file is now evaluated, rather than capacity rejection.
    assert!(matches!(app.reload().await, Outcome::Rejected(_)));
    app.shutdown().await;
    assert!(matches!(app.reload().await, Outcome::Stale(ref reason) if reason.contains("closed")));
}

#[tokio::test]
async fn conditional_reload_checks_version_after_queueing_without_observing_files() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let app = Arc::new(App::new("conditional-queue-unread.kdl".into()).unwrap());
    let expected = app.store.status().version();
    let execution = app.configuration_slot.clone().acquire_owned().await.unwrap();
    let mut queued = Box::pin(app.reload_snapshot_if(expected.clone(), Snapshot::new()));
    let source = SourceSnapshot::new(
        SourceId::file("main"),
        gatel_model::SourceRev(1),
        Hasher::new().finish(),
        vec![],
    );
    let mut queued_source = Box::pin(app.reload_source_if(expected.clone(), source));
    assert!(
        queued_source
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(
        queued
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut newer = Snapshot::new();
    observe(&mut newer, SourceId::file("main"), "not-a-supported-root-node;");
    assert!(matches!(
        app.attempt_snapshot_if(&expected, &newer),
        Ok(Outcome::Rejected(_))
    ));
    let actual = app.store.status().version();
    assert_ne!(actual, expected);
    drop(execution);
    let conflict = tokio::time::timeout(std::time::Duration::from_secs(5), queued)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(conflict.actual, actual);
    let conflict = tokio::time::timeout(std::time::Duration::from_secs(5), queued_source)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(conflict.actual, actual);
    assert_eq!(app.store.status().version(), actual);
    assert!(app.previous.lock().unwrap().is_none());
    assert!(app.server.active_addresses().await.is_empty());
    app.shutdown().await;
    assert!(matches!(
        app.reload_snapshot_if(actual, newer).await,
        Ok(Outcome::Stale(_))
    ));
}

#[tokio::test]
async fn shutdown_does_not_hold_activation_lock_while_draining_admitted_work() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let app = App::new(PathBuf::from("activation-drain-unread.kdl")).unwrap();
    let admission = app.submissions.enter().unwrap();
    let mut shutdown = Box::pin(app.shutdown());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    let finish = async {
        app.activate().await;
        drop(admission);
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(finish, shutdown);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_activates_committed_configuration_after_caller_is_cancelled() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut snapshot = Snapshot::new();
        observe(&mut snapshot, SourceId::file("main"), &format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"owned-activation\"; }} }}"
        ));
        let app = Arc::new(App::from_snapshot(std::env::temp_dir(), snapshot).unwrap());
        app.binds.adopt(socket).unwrap();
        let activation = app.lifecycle.lock().await;
        let caller = { let app = app.clone(); tokio::spawn(async move { app.reload().await }) };
        while app.store.current().is_none() { tokio::task::yield_now().await; }
        assert!(app.server.active_addresses().await.is_empty());
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        drop(activation);
        while app.server.active_addresses().await.is_empty() { tokio::task::yield_now().await; }
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("owned-activation"));
        app.shutdown().await;
        assert!(app.binds.bound().is_empty());
    }).await.unwrap();
}

#[tokio::test]
async fn shutdown_releases_adopted_sockets_even_without_activation() {
    let app = App::new(PathBuf::from("unactivated-listener-unread.kdl")).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = app.binds.adopt(socket).unwrap();
    let weak = Arc::downgrade(&app.binds.get(&address).unwrap());
    app.shutdown().await;
    app.shutdown().await;
    assert!(app.binds.bound().is_empty());
    assert!(weak.upgrade().is_none());
    assert!(std::net::TcpListener::bind(address).is_ok());
}

#[tokio::test]
async fn watcher_shutdown_interrupts_initial_discovery_wait() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let app = Arc::new(App::new(PathBuf::from("watch-stop-unread/root.kdl")).unwrap());
    // A discovery task cannot acquire the execution slot yet.
    let slot = app.configuration_slot.clone().acquire_owned().await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut watcher = Box::pin(crate::watch::reload_until(app.clone(), async {
        let _ = stopped.await;
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(watcher.as_mut().poll(&mut cx).is_pending());
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
        .await
        .unwrap();
    assert!(app.previous.lock().unwrap().is_none());
    assert_eq!(app.configuration_slot.available_permits(), 0);
    drop(slot);
    app.shutdown().await;
}

#[tokio::test]
async fn discovery_shares_the_worker_and_is_refused_after_shutdown() {
    let app = Arc::new(App::new(PathBuf::from("discovery-unread/root.kdl")).unwrap());
    let reactor = std::thread::current().id();
    let paths = app
        .configuration_work(move |app| {
            assert_ne!(std::thread::current().id(), reactor);
            app.watched_paths()
        })
        .await
        .unwrap();
    assert_eq!(paths, vec![PathBuf::from("discovery-unread/root.kdl")]);
    assert!(app.previous.lock().unwrap().is_none());
    app.shutdown().await;
    assert!(
        app.configuration_work(|_| panic!("closed discovery must not run"))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn cancelled_waiter_does_not_release_worker_capacity_or_shutdown_ownership() {
    use std::future::Future;
    use std::task::{Context, Waker};
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let app = Arc::new(App::new(PathBuf::from("blocking-submission-unread.kdl")).unwrap());
    let (release, wait) = std::sync::mpsc::channel();
    let release = Release(Some(release));
    let (started, ready) = tokio::sync::oneshot::channel();
    let caller = {
        let app = app.clone();
        tokio::spawn(async move {
            app.configuration_work(move |_| {
                let _ = started.send(());
                wait.recv().unwrap();
                Outcome::Stale("fixture finished".into())
            })
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), ready)
        .await
        .unwrap()
        .unwrap();
    // This test uses a single-thread async runtime: reaching here while the
    // worker is synchronously blocked also verifies reactor responsiveness.
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(app.configuration_slot.available_permits(), 0);
    let mut cx = Context::from_waker(Waker::noop());
    let mut queued = Box::pin(app.attempt_async());
    assert!(queued.as_mut().poll(&mut cx).is_pending());
    let mut shutdown = Box::pin(app.shutdown());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(queued.await, Outcome::Stale(_)));
    assert!(
        app.previous.lock().unwrap().is_none(),
        "queued work must not read a file"
    );
    drop(release);
    tokio::time::timeout(std::time::Duration::from_secs(3), shutdown)
        .await
        .unwrap();
    assert!(app.submissions.enter().is_none());
}

#[tokio::test]
async fn cancelled_host_shutdown_retries_drain_without_reopening_admission() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let app = App::new(PathBuf::from("submission-drain-unread.kdl")).unwrap();
    // Hold the same admission that surrounds production prepare/publication.
    let admission = app.submissions.enter().unwrap();
    let mut shutdown = Box::pin(app.shutdown());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(app.store.is_sealed());
    assert!(app.submissions.enter().is_none());
    drop(shutdown);
    assert!(app.lifecycle.lock().await.closed);
    let version = app.store.status().version();
    assert!(matches!(
        app.attempt_snapshot_if(&version, &Snapshot::new()),
        Ok(Outcome::Stale(_))
    ));
    let mut retry = Box::pin(app.shutdown());
    assert!(retry.as_mut().poll(&mut cx).is_pending());
    drop(admission);
    tokio::time::timeout(std::time::Duration::from_secs(1), retry)
        .await
        .unwrap();
    assert!(app.server.active_addresses().await.is_empty());
    assert!(app.submissions.enter().is_none());
}

fn observe(snapshot: &mut Snapshot, source: SourceId, text: &str) {
    let expected = snapshot.get(&source).map(|source| source.rev);
    let mut hash = Hasher::new();
    hash.update_str(text);
    snapshot
        .observe_if(
            source,
            expected,
            hash.finish(),
            gatel_kdl::lower(text, "submission").unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn conditional_host_submission_preserves_lkg_and_rejects_obsolete_writers() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let file = SourceId::file("main");
        let admin = SourceId::admin();
        let config = |terminal: &str| {
            format!("global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ {terminal}; }} }}")
        };
        let mut snapshot = Snapshot::new();
        observe(&mut snapshot, file.clone(), &config("respond \"first\""));
        observe(&mut snapshot, admin.clone(), "");
        // Explicit trusted source registration, not an unauthenticated Admin API.
        let app = Arc::new(App::from_snapshot(std::env::temp_dir(), snapshot.clone()).unwrap());
        app.binds.adopt(socket).unwrap();
        let listening = Arc::downgrade(&app.binds.get(&address).unwrap());
        let initial = app.store.status().version();
        assert!(matches!(
            app.reload_snapshot_if(initial, snapshot.clone()).await,
            Ok(Outcome::Started { .. })
        ));
        let request = || async {
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8(bytes).unwrap()
        };
        assert!(request().await.ends_with("first"));
        let first = app.store.current().unwrap();
        let expected = app.store.status().version();
        observe(
            &mut snapshot,
            file.clone(),
            &config("proxy upstream=\"runtime/missing\""),
        );
        assert!(matches!(
            app.reload_snapshot_if(expected.clone(), snapshot.clone()).await,
            Ok(Outcome::Rejected(_))
        ));
        assert!(Arc::ptr_eq(&first, &app.store.current().unwrap()));
        assert!(request().await.ends_with("first"));
        let failed = app.store.status().version();
        // The rejected intent changes the write validator even without a new runtime.
        let conflict = app
            .reload_snapshot_if(expected, snapshot.clone())
            .await
            .unwrap_err();
        assert_eq!(conflict.actual, failed);
        assert_eq!(app.store.status().version(), failed);
        observe(&mut snapshot, file, &config("respond \"recovered\""));
        observe(
            &mut snapshot,
            admin,
            "upstream \"runtime/shared\" { endpoint \"127.0.0.1:1\"; }",
        );
        assert!(matches!(
            app.reload_snapshot_if(failed.clone(), snapshot.clone()).await,
            Ok(Outcome::Reloaded { .. })
        ));
        assert!(request().await.ends_with("recovered"));
        assert_eq!(app.store.current().unwrap().plan.upstreams.len(), 1);
        let published = app.store.status().version();
        assert!(app.reload_snapshot_if(failed, snapshot.clone()).await.is_err());
        assert_eq!(app.store.status().version(), published);
        assert!(matches!(
            app.reload_snapshot_if(published, snapshot.clone()).await,
            Ok(Outcome::NoOp { .. })
        ));
        app.shutdown().await;
        assert!(app.binds.bound().is_empty());
        assert!(
            listening.upgrade().is_none(),
            "closed App still owns a listening socket"
        );
        assert!(app.store.is_sealed());
        let stopped = app.store.current().unwrap();
        let version = app.store.status().version();
        observe(
            &mut snapshot,
            SourceId::file("main"),
            &config("respond \"must-not-publish\""),
        );
        assert!(matches!(
            app.attempt_snapshot_if(&version, &snapshot),
            Ok(Outcome::Stale(_))
        ));
        assert_eq!(app.store.status().version(), version);
        assert!(Arc::ptr_eq(&stopped, &app.store.current().unwrap()));
        app.activate().await;
        assert!(app.server.active_addresses().await.is_empty());
    })
    .await
    .unwrap();
}
