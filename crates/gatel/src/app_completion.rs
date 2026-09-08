//! Blocking receipt work for an already-admitted, owned reload transaction.
use std::sync::Arc;

use gatel_plan::Outcome;

use super::App;

impl App {
    /// Unlike configuration_work, completion must not attempt fresh admission:
    /// shutdown may have closed it after publication. The enclosing reload job
    /// retains its admission and execution guard until this worker is joined.
    /// One serialized reload can enqueue at most one completion worker per App.
    /// No generation or lifecycle lock is held while the callback performs I/O.
    #[allow(
        dead_code,
        reason = "Host persistence adapter entry; disk policy is not yet exposed"
    )]
    pub(super) async fn reload_task_persisting<F, Fut, E, Finish>(
        self: &Arc<Self>,
        work: F,
        finish: Finish,
    ) -> Result<Outcome, E>
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Outcome, E>> + Send,
        E: Send + 'static,
        Finish: FnOnce(&App, Result<Outcome, E>) -> Result<Outcome, E> + Send + 'static,
    {
        self.reload_task_finishing(work, move |app, result| async move {
            // The worker is never exposed to the request, and shutdown waits
            // on the parent admission. There is deliberately no sealed check:
            // a sealed store still needs its already-started write classified.
            match tokio::task::spawn_blocking(move || finish(&app, result)).await {
                Ok(result) => result,
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(_) => panic!("configuration completion worker cancelled unexpectedly"),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "admin-api")]
    #[tokio::test]
    async fn blocking_completion_checkpoints_serving_and_rejected_observation() {
        use gatel_model::{BootId, Hasher, Snapshot, SourceId, SourceRev, SourceSnapshot};

        use crate::admin::CheckpointWriter;
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            Directory(std::env::temp_dir().join(format!("gatel-completion-disk-{}", BootId::generate())));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&directory.0).unwrap();
        let app = Arc::new(App::with_sources(std::env::temp_dir(), [SourceId::file("main")]).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        app.binds.adopt(listener).unwrap();
        let (sink, previous) =
            CheckpointWriter::open(&directory.0, "completion-fixture", &app.registry).unwrap();
        assert!(previous.is_none());
        let sink = Arc::new(std::sync::Mutex::new(sink));
        let initial = app.store.status().version();
        for (revision, text) in [
            (
                1,
                format!(
                    "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"durable-fixture\"; }} }}"
                ),
            ),
            (2, "invalid-directive \"rejected-secret\";".into()),
        ] {
            let mut hash = Hasher::new();
            hash.update_str(&text);
            let snapshot = Snapshot::new().with(SourceSnapshot::new(
                SourceId::file("main"),
                SourceRev(revision),
                hash.finish(),
                gatel_kdl::lower(&text, "fixture").unwrap(),
            ));
            let outcome = app
                .reload_checkpoint_if(app.store.status().version(), snapshot, sink.clone())
                .await
                .unwrap();
            assert!(if revision == 1 {
                matches!(outcome, Outcome::Started { .. })
            } else {
                matches!(outcome, Outcome::Rejected(_))
            });
        }
        let checkpoint_bytes = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
        let version = app.store.status().version();
        assert!(matches!(
            app.reload_checkpoint_if(initial, Snapshot::new(), sink.clone())
                .await,
            Err(gatel_plan::SourceWriteError::Conflict(_))
        ));
        assert_eq!(app.store.status().version(), version);
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            checkpoint_bytes
        );
        sink.lock().unwrap().require_reconciliation();
        assert!(matches!(
            app.reload_checkpoint_if(version.clone(), Snapshot::new(), sink.clone())
                .await,
            Err(gatel_plan::SourceWriteError::PersistenceUnknown)
        ));
        assert_eq!(app.store.status().version(), version);
        assert_eq!(
            std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
            checkpoint_bytes
        );
        assert!(CheckpointWriter::open(&directory.0, "completion-fixture", &app.registry).is_err());
        drop(sink);
        let (_sink, snapshots) =
            CheckpointWriter::open(&directory.0, "completion-fixture", &app.registry).unwrap();
        let (serving, observed) = snapshots.unwrap();
        assert_eq!(serving.get(&SourceId::file("main")).unwrap().rev, SourceRev(1));
        assert_eq!(observed.get(&SourceId::file("main")).unwrap().rev, SourceRev(2));
        assert!(gatel_plan::normalize(&serving, &app.registry).is_ok());
        assert!(gatel_plan::normalize(&observed, &app.registry).is_err());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            client
                .take(16 * 1024)
                .read_to_string(&mut response)
                .await
                .unwrap();
            assert!(
                response.starts_with("HTTP/1.1 200") && response.ends_with("durable-fixture"),
                "{response}"
            );
        })
        .await
        .unwrap();
        app.shutdown().await;
    }
    use std::future::Future;
    use std::task::{Context, Waker};
    use std::time::Duration;

    #[tokio::test]
    async fn admitted_completion_runs_after_shutdown_closes_new_work() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let app = Arc::new(App::new("completion-shutdown-unread.kdl".into()).unwrap());
            let (entered, received) = tokio::sync::oneshot::channel();
            let (continue_work, work_ready) = tokio::sync::oneshot::channel();
            let (worker_entered, worker_received) = tokio::sync::oneshot::channel();
            let (release, resume) = std::sync::mpsc::channel();
            let owner = app.clone();
            let reactor = std::thread::current().id();
            let waiter = tokio::spawn(async move {
                owner
                    .reload_task_persisting(
                        move |_| async move {
                            entered.send(()).unwrap();
                            work_ready.await.unwrap();
                            Err::<Outcome, _>(gatel_plan::SourceWriteError::PersistenceUnknown)
                        },
                        move |app, result| {
                            assert_ne!(std::thread::current().id(), reactor);
                            assert!(app.store.is_sealed());
                            assert!(app.configuration_slot.is_closed());
                            assert!(app.reload_execution.try_lock().is_err());
                            assert!(app.lifecycle.try_lock().is_ok());
                            // Capturing both source snapshots needs a store read
                            // lock; it must not deadlock behind the host callback.
                            assert!(app.store.capture_configuration().is_sealed());
                            worker_entered.send(()).unwrap();
                            resume.recv_timeout(Duration::from_secs(5)).unwrap();
                            result
                        },
                    )
                    .await
            });
            received.await.unwrap();
            let mut shutdown = Box::pin(app.shutdown());
            assert!(
                shutdown
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(app.configuration_work(|_| ()).await.is_none());
            continue_work.send(()).unwrap();
            worker_received.await.unwrap();
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            assert!(
                shutdown
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(
                app.reload_slots.available_permits(),
                super::super::MAX_RELOAD_TASKS - 1
            );
            release.send(()).unwrap();
            shutdown.await;
            assert!(app.reload_execution.try_lock().is_ok());
            assert_eq!(
                app.reload_slots.available_permits(),
                super::super::MAX_RELOAD_TASKS
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn panicking_completion_propagates_and_releases_admission() {
        let app = Arc::new(App::new("completion-panic-unread.kdl".into()).unwrap());
        let owner = app.clone();
        let task = tokio::spawn(async move {
            owner
                .reload_task_persisting(
                    |_| async { Err::<Outcome, ()>(()) },
                    |_, _| panic!("injected completion panic"),
                )
                .await
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(app.reload_execution.try_lock().is_ok());
        assert_eq!(
            app.reload_slots.available_permits(),
            super::super::MAX_RELOAD_TASKS
        );
        app.shutdown().await;
    }
}
