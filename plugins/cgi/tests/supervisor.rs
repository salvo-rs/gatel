use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Waker};

use gatel_cgi::exchange;
use gatel_cgi::supervisor::{self, Error, Job};
use gatel_plugin_runtime::ShutdownSignal;
use tokio::sync::oneshot;

struct Work {
    runs: Arc<AtomicUsize>,
    cleanup_started: Option<oneshot::Sender<()>>,
    cleanup_done: oneshot::Receiver<bool>,
    finish: bool,
    panic: bool,
}
impl Job for Work {
    async fn run(&mut self) -> Result<u64, exchange::Error> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "test job panic");
        if self.finish {
            Ok(7)
        } else {
            std::future::pending().await
        }
    }
    async fn cleanup(&mut self) -> io::Result<()> {
        if let Some(started) = self.cleanup_started.take() {
            let _ = started.send(());
        }
        if (&mut self.cleanup_done).await.unwrap_or(false) {
            Ok(())
        } else {
            Err(io::Error::other("private cleanup error"))
        }
    }
}
fn work(finish: bool) -> (Work, oneshot::Receiver<()>, oneshot::Sender<bool>) {
    let (started, observed) = oneshot::channel();
    let (done, waited) = oneshot::channel();
    (
        Work {
            runs: Arc::new(AtomicUsize::new(0)),
            cleanup_started: Some(started),
            cleanup_done: waited,
            finish,
            panic: false,
        },
        observed,
        done,
    )
}
fn poll_pending<F: Future>(future: std::pin::Pin<&mut F>) {
    assert!(future.poll(&mut Context::from_waker(Waker::noop())).is_pending());
}

#[tokio::test]
async fn cancellation_retains_capacity_until_cleanup_and_then_allows_reuse() {
    let (client, owner) = supervisor::channel::<Work>(1).unwrap();
    let (stop, shutdown) = ShutdownSignal::new();
    let mut owner = Box::pin(owner.run(shutdown));
    let (job, cleaning, release) = work(false);
    let runs = job.runs.clone();
    let mut request = Box::pin(client.execute(job));
    poll_pending(request.as_mut());
    poll_pending(owner.as_mut());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    drop(request);
    poll_pending(owner.as_mut());
    cleaning.await.unwrap();
    assert!(matches!(client.execute(work(true).0).await, Err(Error::Busy)));
    release.send(true).unwrap();
    poll_pending(owner.as_mut());
    let (job, _, release) = work(true);
    release.send(true).unwrap();
    let mut request = Box::pin(client.execute(job));
    poll_pending(request.as_mut());
    poll_pending(owner.as_mut());
    assert_eq!(request.await.unwrap(), 7);
    stop.request();
    owner.await.unwrap();
}

#[tokio::test]
async fn preparation_is_inert_and_cancelled_queued_work_never_starts() {
    let (client, owner) = supervisor::channel::<Work>(1).unwrap();
    let (job, ..) = work(false);
    let runs = job.runs.clone();
    let mut request = Box::pin(client.execute(job));
    poll_pending(request.as_mut());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(matches!(client.execute(work(true).0).await, Err(Error::Busy)));
    drop(request);
    let (stop, shutdown) = ShutdownSignal::new();
    let mut running = Box::pin(owner.run(shutdown));
    poll_pending(running.as_mut());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    stop.request();
    running.await.unwrap();
    assert!(matches!(client.execute(work(true).0).await, Err(Error::Closed)));
}

#[tokio::test]
async fn shutdown_cancels_all_jobs_and_waits_for_all_cleanups() {
    let (client, owner) = supervisor::channel::<Work>(2).unwrap();
    let (stop, shutdown) = ShutdownSignal::new();
    let mut owner = Box::pin(owner.run(shutdown));
    let (one, clean_one, release_one) = work(false);
    let (two, clean_two, release_two) = work(false);
    let mut first = Box::pin(client.execute(one));
    let mut second = Box::pin(client.execute(two));
    poll_pending(first.as_mut());
    poll_pending(second.as_mut());
    poll_pending(owner.as_mut());
    stop.request();
    poll_pending(owner.as_mut());
    clean_one.await.unwrap();
    clean_two.await.unwrap();
    release_one.send(true).unwrap();
    poll_pending(owner.as_mut());
    assert!(matches!(first.await, Err(Error::Cancelled)));
    release_two.send(true).unwrap();
    owner.await.unwrap();
    assert!(matches!(second.await, Err(Error::Cancelled)));
}

#[tokio::test]
async fn run_panic_is_cleaned_and_cleanup_failure_seals_generation() {
    for panic in [false, true] {
        let (client, owner) = supervisor::channel::<Work>(1).unwrap();
        let (_stop, shutdown) = ShutdownSignal::new();
        let (mut job, cleaning, release) = work(true);
        job.panic = panic;
        let mut request = Box::pin(client.execute(job));
        poll_pending(request.as_mut());
        let mut owner = Box::pin(owner.run(shutdown));
        poll_pending(owner.as_mut());
        cleaning.await.unwrap();
        release.send(false).unwrap();
        assert!(owner.await.unwrap_err().contains("generation sealed"));
        assert!(matches!(request.await, Err(Error::Cleanup)));
        assert!(matches!(client.execute(work(true).0).await, Err(Error::Closed)));
    }
}

#[tokio::test]
async fn dropping_unactivated_supervisor_closes_admission() {
    assert!(supervisor::channel::<Work>(0).is_err());
    assert!(supervisor::channel::<Work>(1025).is_err());
    let (client, owner) = supervisor::channel::<Work>(1).unwrap();
    drop(owner);
    assert!(matches!(client.execute(work(true).0).await, Err(Error::Closed)));
}

#[tokio::test]
async fn completion_and_panic_results_wait_for_cleanup() {
    for panic in [false, true] {
        let (client, owner) = supervisor::channel::<Work>(1).unwrap();
        let (stop, shutdown) = ShutdownSignal::new();
        let (mut job, cleaning, release) = work(true);
        job.panic = panic;
        let mut request = Box::pin(client.execute(job));
        poll_pending(request.as_mut());
        let mut owner = Box::pin(owner.run(shutdown));
        poll_pending(owner.as_mut());
        cleaning.await.unwrap();
        poll_pending(request.as_mut());
        assert!(matches!(client.execute(work(true).0).await, Err(Error::Busy)));
        release.send(true).unwrap();
        poll_pending(owner.as_mut());
        let result = request.await;
        if panic {
            assert!(matches!(result, Err(Error::Panicked)));
        } else {
            assert_eq!(result.unwrap(), 7);
        }
        // A run panic with verified cleanup does not poison this generation.
        let (job, _, release) = work(true);
        release.send(true).unwrap();
        let mut next = Box::pin(client.execute(job));
        poll_pending(next.as_mut());
        poll_pending(owner.as_mut());
        assert_eq!(next.await.unwrap(), 7);
        stop.request();
        owner.await.unwrap();
    }
}

#[tokio::test]
async fn compatible_generations_share_cleaning_permits_but_not_retirement() {
    let pool = Arc::new(tokio::sync::Semaphore::new(1));
    let (old, owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (next, next_owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (stop, shutdown) = ShutdownSignal::new();
    let mut owner = Box::pin(owner.run(shutdown));
    let (job, cleaning, release) = work(false);
    let mut request = Box::pin(old.execute(job));
    poll_pending(request.as_mut());
    poll_pending(owner.as_mut());
    stop.request();
    poll_pending(owner.as_mut());
    cleaning.await.unwrap();
    assert!(matches!(next.execute(work(true).0).await, Err(Error::Busy)));
    assert!(!pool.is_closed());
    release.send(true).unwrap();
    owner.await.unwrap();
    assert!(matches!(request.await, Err(Error::Cancelled)));
    assert!(matches!(old.execute(work(true).0).await, Err(Error::Closed)));
    let (stop, shutdown) = ShutdownSignal::new();
    let mut owner = Box::pin(next_owner.run(shutdown));
    let (job, _, release) = work(true);
    release.send(true).unwrap();
    let mut request = Box::pin(next.execute(job));
    poll_pending(request.as_mut());
    poll_pending(owner.as_mut());
    assert_eq!(request.await.unwrap(), 7);
    stop.request();
    owner.await.unwrap();
    assert_eq!(pool.available_permits(), 1);
    assert!(!pool.is_closed());
}

#[tokio::test]
async fn failed_cleanup_poisoning_is_shared_but_unactivated_drop_is_not() {
    let pool = Arc::new(tokio::sync::Semaphore::new(1));
    let (_, discarded) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    drop(discarded);
    assert!(!pool.is_closed());
    let (old, owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (next, _next_owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (_stop, shutdown) = ShutdownSignal::new();
    let (job, _, release) = work(true);
    release.send(false).unwrap();
    let mut request = Box::pin(old.execute(job));
    poll_pending(request.as_mut());
    assert!(owner.run(shutdown).await.is_err());
    assert!(matches!(request.await, Err(Error::Cleanup)));
    assert!(pool.is_closed());
    assert!(matches!(next.execute(work(true).0).await, Err(Error::Closed)));
}

#[tokio::test]
async fn forced_owner_abort_poisoning_precedes_shared_permit_release() {
    let pool = Arc::new(tokio::sync::Semaphore::new(1));
    let (old, owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (next, _next_owner) = supervisor::channel_with_capacity::<Work>(1, pool.clone()).unwrap();
    let (_stop, shutdown) = ShutdownSignal::new();
    let mut owner = Box::pin(owner.run(shutdown));
    let (job, _, _release) = work(false);
    let runs = job.runs.clone();
    let mut request = Box::pin(old.execute(job));
    poll_pending(request.as_mut());
    poll_pending(owner.as_mut());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    drop(owner);
    assert!(pool.is_closed());
    assert!(matches!(request.await, Err(Error::Closed)));
    assert!(matches!(next.execute(work(true).0).await, Err(Error::Closed)));
}
