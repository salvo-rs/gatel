use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Context;

use gatel_plan::BackgroundTask;

use super::*;
use crate::plugin_tasks::{PluginTasks, TaskOutcome};

#[tokio::test]
async fn prepare_reserves_capacity_and_rejected_candidates_never_start_work() {
    let owner = Arc::new(Retirements::default());
    let candidate = || {
        PluginTasks::managed(
            vec![BackgroundTask::new("inert", || async {
                panic!("discarded candidate was activated");
            })],
            &owner,
        )
    };
    let mut scopes = (0..Retirements::LIMIT)
        .map(|_| candidate().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(owner.tracked_generations(), Retirements::LIMIT);
    assert!(candidate().is_err());
    drop(scopes.pop());
    scopes.push(candidate().unwrap());
    owner.shutdown().await;
    assert_eq!(owner.tracked_generations(), 0);
    assert!(candidate().is_err());
    for scope in scopes {
        scope.activate();
    }
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn retired_work_keeps_capacity_until_async_cleanup_finishes() {
    let owner = Arc::new(Retirements::default());
    let (ready, started) = tokio::sync::oneshot::channel();
    let (cleaning, wait) = tokio::sync::oneshot::channel();
    let (release, hold) = tokio::sync::oneshot::channel();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let observed = cleaned.clone();
    let scope = PluginTasks::managed(
        vec![BackgroundTask::cooperative(
            "retire",
            move |mut shutdown| async move {
                ready.send(()).unwrap();
                shutdown.requested().await;
                cleaning.send(()).unwrap();
                hold.await.unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )],
        &owner,
    )
    .unwrap();
    scope.activate();
    started.await.unwrap();
    drop(scope);
    wait.await.unwrap();
    assert_eq!(owner.tracked_generations(), 1);
    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    owner.shutdown().await;
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(owner.tracked_generations(), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_drains_pinned_scopes_together_and_blocks_reactivation() {
    let owner = Arc::new(Retirements::default());
    let (ready, mut started) = tokio::sync::mpsc::channel(3);
    let scopes = (1..=3)
        .map(|seconds| {
            let ready = ready.clone();
            let scope = PluginTasks::managed(
                vec![BackgroundTask::cooperative(
                    "cleanup",
                    move |mut shutdown| async move {
                        ready.send(()).await.unwrap();
                        shutdown.requested().await;
                        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                        Ok(())
                    },
                )],
                &owner,
            )
            .unwrap();
            scope.activate();
            scope
        })
        .collect::<Vec<_>>();
    for _ in 0..3 {
        started.recv().await.unwrap();
    }
    let before = tokio::time::Instant::now();
    owner.shutdown().await;
    assert_eq!(before.elapsed(), std::time::Duration::from_secs(3));
    for scope in &scopes {
        assert_eq!(scope.outcomes(), vec![("cleanup", TaskOutcome::Completed)]);
        scope.activate();
    }
    assert_eq!(owner.tracked_generations(), 0);
}

#[tokio::test]
async fn cancelled_manager_shutdown_can_retry_without_losing_retirement_jobs() {
    let owner = Arc::new(Retirements::default());
    let (ready, started) = tokio::sync::oneshot::channel();
    let (cleaning, wait) = tokio::sync::oneshot::channel();
    let (release, hold) = tokio::sync::oneshot::channel();
    let scope = PluginTasks::managed(
        vec![BackgroundTask::cooperative(
            "cleanup",
            move |mut shutdown| async move {
                ready.send(()).unwrap();
                shutdown.requested().await;
                cleaning.send(()).unwrap();
                hold.await.unwrap();
                Ok(())
            },
        )],
        &owner,
    )
    .unwrap();
    scope.activate();
    started.await.unwrap();
    drop(scope);
    wait.await.unwrap();
    let mut closing = Box::pin(owner.shutdown());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(closing.as_mut().poll(&mut cx).is_pending());
    drop(closing);
    assert_eq!(owner.tracked_generations(), 1);
    release.send(()).unwrap();
    owner.shutdown().await;
    assert_eq!(owner.tracked_generations(), 0);
}
