use std::future::Future;
use std::task::{Context, Waker};
use std::time::Duration;

use super::*;

#[tokio::test]
async fn completion_owns_order_and_admission_after_waiter_cancellation() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let app = Arc::new(App::new("completion-unread.kdl".into()).unwrap());
        let (entered, received) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = completed.clone();
        let owner = app.clone();
        let waiter = tokio::spawn(async move {
            owner
                .reload_task_finishing(
                    |_| async { Err::<Outcome, _>("work failed after intent") },
                    move |_, result| async move {
                        assert!(matches!(result, Err("work failed after intent")));
                        entered.send(()).unwrap();
                        resume.await.unwrap();
                        marker.store(true, std::sync::atomic::Ordering::Release);
                        Err("persistence outcome unknown")
                    },
                )
                .await
        });
        received.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(app.reload_execution.try_lock().is_err());
        assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS - 1);
        let (next_entered, mut next_received) = tokio::sync::oneshot::channel();
        let mut next = Box::pin(app.reload_task(move |_| async move {
            assert!(completed.load(std::sync::atomic::Ordering::Acquire));
            next_entered.send(()).unwrap();
            Ok::<_, &str>(Outcome::Stale("fixture".into()))
        }));
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(next_received.try_recv().is_err());
        release.send(()).unwrap();
        next.await.unwrap();
        next_received.await.unwrap();
        app.submissions.drained().await;
        assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn completion_error_is_returned_without_reclassifying_as_unapplied() {
    let app = Arc::new(App::new("completion-error-unread.kdl".into()).unwrap());
    let result = app
        .reload_task_finishing(
            |_| async { Ok::<_, gatel_plan::SourceWriteError>(Outcome::Stale("fixture".into())) },
            |_, _| async { Err(gatel_plan::SourceWriteError::PersistenceUnknown) },
        )
        .await;
    assert!(matches!(
        result,
        Err(gatel_plan::SourceWriteError::PersistenceUnknown)
    ));
    assert!(app.reload_execution.try_lock().is_ok());
    assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
    app.shutdown().await;
}

#[tokio::test]
async fn owned_reload_gate_extends_past_publication_until_activation_finishes() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let text = format!(
            "global {{ http \"{address}\"; }}\nsite \"*\" {{\nroute \"/\" {{ respond \"first\"; }}\n}}"
        );
        let mut hash = gatel_model::Hasher::new();
        hash.update_str(&text);
        let snapshot = Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            gatel_model::SourceRev(1),
            hash.finish(),
            gatel_kdl::lower(&text, "fixture").unwrap(),
        ));
        let app = Arc::new(App::from_snapshot(std::env::temp_dir(), snapshot).unwrap());
        app.binds.adopt(socket).unwrap();
        let activation = app.lifecycle.lock().await;
        let first = {
            let app = app.clone();
            tokio::spawn(async move {
                app.reload_task_finishing(
                    |app| async move { Ok::<_, ()>(app.attempt_async().await) },
                    |app, result| async move {
                        assert_eq!(
                            app.activated.load(std::sync::atomic::Ordering::Acquire),
                            app.store.current().unwrap().number.number()
                        );
                        assert!(app.reload_execution.try_lock().is_err());
                        result
                    },
                )
                .await
                .unwrap()
            })
        };
        while app.store.current().is_none() {
            tokio::task::yield_now().await;
        }
        assert_eq!(app.activated.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(
            app.reload_execution.try_lock().is_err(),
            "publication released ordering before activation"
        );
        let (entered, mut received) = tokio::sync::oneshot::channel();
        let mut second = Box::pin(app.reload_task(move |app| async move {
            entered.send(()).unwrap();
            assert_eq!(
                app.activated.load(std::sync::atomic::Ordering::Acquire),
                app.store.current().unwrap().number.number()
            );
            Ok::<_, ()>(Outcome::NoOp {
                generation: app.store.current().unwrap().number,
            })
        }));
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(second); // Cancellation cannot remove this accepted job or its slot.
        assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS - 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut received)
                .await
                .is_err()
        );
        drop(activation);
        assert!(matches!(first.await.unwrap(), Outcome::Started { .. }));
        received.await.unwrap();
        app.submissions.drained().await;
        assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
        assert!(app.reload_execution.try_lock().is_ok());
        assert_eq!(app.server.active_addresses().await, vec![address]);
        app.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shutdown_refuses_queued_execution_without_running_its_work() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let app = Arc::new(App::new("execution-shutdown-unread.kdl".into()).unwrap());
        let gate = app.reload_execution.lock().await;
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut jobs = Vec::new();
        for _ in 0..MAX_RELOAD_TASKS {
            let entered = entered.clone();
            let mut job = Box::pin(app.reload_task_finishing(
                move |_| async move {
                    entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok::<_, ()>(Outcome::Stale("should not run".into()))
                },
                |_, _| async { panic!("rejected admission must not run completion") },
            ));
            assert!(
                job.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            jobs.push(job);
        }
        let mut shutdown = Box::pin(app.shutdown());
        assert!(
            shutdown
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(app.store.is_sealed());
        drop(gate);
        shutdown.await;
        for job in jobs {
            assert!(matches!(job.await, Ok(Outcome::Stale(reason)) if reason.contains("closed")));
        }
        assert_eq!(entered.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
        assert!(app.reload_execution.try_lock().is_ok());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_owned_work_releases_execution_gate_and_admission() {
    let app = Arc::new(App::new("execution-failure-unread.kdl".into()).unwrap());
    let failure = app
        .reload_task(|_| async { Err::<Outcome, _>("fixture failure") })
        .await;
    assert!(matches!(failure, Err("fixture failure")));
    assert!(app.reload_execution.try_lock().is_ok());
    assert_eq!(app.reload_slots.available_permits(), MAX_RELOAD_TASKS);
    assert!(matches!(app.reload().await, Outcome::Rejected(_)));
    app.shutdown().await;
}
