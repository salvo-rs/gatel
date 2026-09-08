use std::future::Future;
use std::sync::atomic::AtomicUsize;
use std::task::Context;
use std::time::Duration;

use super::*;

struct OnDrop(Arc<AtomicUsize>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn cooperative_cleanup_is_signalled_together_and_joined_before_return() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (ready, mut started) = tokio::sync::mpsc::channel(3);
    let tasks = (1..=3)
        .map(|seconds| {
            let dropped = dropped.clone();
            let ready = ready.clone();
            BackgroundTask::cooperative("cleanup", move |mut shutdown| async move {
                let _guard = OnDrop(dropped);
                ready.send(()).await.unwrap();
                shutdown.requested().await;
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                Ok(())
            })
        })
        .collect();
    let scope = PluginTasks::new(tasks).unwrap();
    scope.activate();
    for _ in 0..3 {
        started.recv().await.unwrap();
    }
    let before = tokio::time::Instant::now();
    scope.shutdown().await;
    assert_eq!(before.elapsed(), Duration::from_secs(3));
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
    assert!(
        scope
            .outcomes()
            .iter()
            .all(|(_, outcome)| *outcome == TaskOutcome::Completed)
    );
}

#[tokio::test(start_paused = true)]
async fn grace_is_one_shared_budget_and_ignoring_shutdown_cannot_detach_work() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (ready, mut started) = tokio::sync::mpsc::channel(3);
    let tasks = (0..3)
        .map(|_| {
            let dropped = dropped.clone();
            let ready = ready.clone();
            BackgroundTask::cooperative("stuck", move |_shutdown| async move {
                let _guard = OnDrop(dropped);
                ready.send(()).await.unwrap();
                std::future::pending::<()>().await;
                Ok(())
            })
        })
        .collect();
    let scope = PluginTasks::new(tasks).unwrap();
    scope.activate();
    for _ in 0..3 {
        started.recv().await.unwrap();
    }
    let before = tokio::time::Instant::now();
    scope.shutdown().await;
    assert_eq!(before.elapsed(), PluginTasks::SHUTDOWN_GRACE);
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
    assert!(
        scope
            .outcomes()
            .iter()
            .all(|(_, outcome)| *outcome == TaskOutcome::Cancelled)
    );
}

#[tokio::test]
async fn concurrent_shutdown_callers_both_wait_for_cleanup() {
    let (cleaning, wait) = tokio::sync::oneshot::channel();
    let (release, hold) = tokio::sync::oneshot::channel();
    let scope = PluginTasks::new(vec![BackgroundTask::cooperative(
        "cleanup",
        move |mut shutdown| async move {
            shutdown.requested().await;
            cleaning.send(()).unwrap();
            hold.await.unwrap();
            Ok(())
        },
    )])
    .unwrap();
    scope.activate();
    let mut first = Box::pin(scope.shutdown());
    tokio::select! {
        value = wait => value.unwrap(),
        _ = &mut first => panic!("shutdown returned before cleanup"),
    }
    let mut second = Box::pin(scope.shutdown());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    release.send(()).unwrap();
    tokio::join!(first, second);
    assert_eq!(scope.outcomes(), vec![("cleanup", TaskOutcome::Completed)]);
}

#[tokio::test]
async fn cancelling_shutdown_retains_remaining_join_handles_for_retry() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (cleaning, wait) = tokio::sync::oneshot::channel();
    let tracked = dropped.clone();
    let scope = PluginTasks::new(vec![
        BackgroundTask::cooperative("done", |mut shutdown| async move {
            shutdown.requested().await;
            Ok(())
        }),
        BackgroundTask::cooperative("cleanup", move |mut shutdown| async move {
            let _guard = OnDrop(tracked);
            shutdown.requested().await;
            cleaning.send(()).unwrap();
            std::future::pending::<()>().await;
            Ok(())
        }),
    ])
    .unwrap();
    scope.activate();
    let mut first = Box::pin(scope.shutdown());
    tokio::select! {
        value = wait => value.unwrap(),
        _ = &mut first => panic!("shutdown returned before cleanup"),
    }
    // Consume the completed first handle before cancelling the remaining join.
    while scope.outcomes()[0].1 != TaskOutcome::Completed {
        tokio::task::yield_now().await;
    }
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    drop(first);
    assert!(matches!(*scope.inner.state.lock().unwrap(), State::Stopping(_)));
    scope.shutdown().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        scope.outcomes(),
        vec![
            ("done", TaskOutcome::Completed),
            ("cleanup", TaskOutcome::Cancelled)
        ]
    );
    scope.shutdown().await;
}
