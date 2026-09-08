use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[tokio::test(start_paused = true)]
async fn panic_and_unexpected_return_restart_with_capped_backoff() {
    let (stop, receiver) = watch::channel(false);
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let mut attempt = 0;
    let task = tokio::spawn(supervise(
        WorkerKind::Probe,
        "test".into(),
        Telemetry::new(),
        receiver,
        move |_| {
            attempt += 1;
            let started = started.clone();
            async move {
                started.send(tokio::time::Instant::now()).unwrap();
                assert!(attempt % 2 == 0, "injected child panic");
            }
        },
    ));
    let mut previous = starts.recv().await.unwrap();
    for seconds in [1, 2, 4, 8, 16, 30, 30] {
        let current = starts.recv().await.unwrap();
        assert_eq!(current.duration_since(previous), Duration::from_secs(seconds));
        previous = current;
    }
    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(starts.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_stable_run_resets_the_backoff() {
    let (stop, receiver) = watch::channel(false);
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let mut attempt = 0;
    let task = tokio::spawn(supervise(
        WorkerKind::Probe,
        "test".into(),
        Telemetry::new(),
        receiver,
        move |_| {
            attempt += 1;
            let started = started.clone();
            async move {
                started.send(tokio::time::Instant::now()).unwrap();
                if attempt == 3 {
                    tokio::time::sleep(STABLE_AFTER).await;
                }
            }
        },
    ));
    let mut previous = starts.recv().await.unwrap();
    for seconds in [1, 2, 61, 2] {
        let current = starts.recv().await.unwrap();
        assert_eq!(current.duration_since(previous), Duration::from_secs(seconds));
        previous = current;
    }
    stop.send(true).unwrap();
    task.await.unwrap();
}

struct Dropped(Arc<AtomicUsize>);

#[tokio::test(start_paused = true)]
async fn backoff_metrics_are_released_on_stop_and_abort_without_counting_a_restart() {
    for abort in [false, true] {
        let telemetry = Telemetry::new();
        let (stop, receiver) = watch::channel(false);
        let task = tokio::spawn(supervise(
            WorkerKind::Resolver,
            "not-a-metric-label".into(),
            Arc::clone(&telemetry),
            receiver,
            |_| std::future::ready(()),
        ));
        tokio::task::yield_now().await;
        let metrics = telemetry.workers.get(WorkerKind::Resolver);
        assert_eq!(metrics.starts.get(), 1);
        assert_eq!(metrics.returned.get(), 1);
        assert_eq!(metrics.panicked.get(), 0);
        assert!(
            gatel_observe::prometheus(&telemetry).contains("gatel_workers_backoff{service=\"resolver\"} 1\n")
        );
        if abort {
            task.abort();
        } else {
            stop.send(true).unwrap();
        }
        let result = task.await;
        if abort {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            result.unwrap();
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(
            metrics.starts.get(),
            1,
            "cancelled backoff must not start another child"
        );
        let rendered = gatel_observe::prometheus(&telemetry);
        assert!(rendered.contains("gatel_workers_backoff{service=\"resolver\"} 0\n"));
        assert!(!rendered.contains("not-a-metric-label"));
    }
}

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn stop_close_and_owner_abort_drop_the_active_child() {
    for mode in 0..3 {
        let (stop, receiver) = watch::channel(false);
        let dropped = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&dropped);
        let (ready, mut started) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(supervise(
            WorkerKind::Probe,
            "test".into(),
            Telemetry::new(),
            receiver,
            move |_| {
                let guard = Dropped(Arc::clone(&counter));
                ready.send(()).unwrap();
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                }
            },
        ));
        started.recv().await.unwrap();
        match mode {
            0 => stop.send(true).unwrap(),
            1 => drop(stop),
            _ => task.abort(),
        }
        let result = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap();
        if mode == 2 {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            result.unwrap();
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(started.recv().await.is_none());
    }
}

#[tokio::test]
async fn a_pre_stopped_worker_does_not_construct_a_child() {
    let (_stop, receiver) = watch::channel(true);
    supervise(
        WorkerKind::Probe,
        "test".into(),
        Telemetry::new(),
        receiver,
        |_| {
            panic!("must not construct a child");
            #[allow(unreachable_code)]
            std::future::ready(())
        },
    )
    .await;
}
