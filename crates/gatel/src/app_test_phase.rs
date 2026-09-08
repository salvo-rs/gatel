//! Test-only wall-clock breadcrumbs and cooperative async deadlines.
use std::future::Future;
use std::time::{Duration, Instant};

pub(super) struct Phase {
    name: &'static str,
    started: Instant,
}
impl Phase {
    pub(super) fn new(name: &'static str) -> Self {
        eprintln!("watcher fixture phase={name} started");
        Self {
            name,
            started: Instant::now(),
        }
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        eprintln!(
            "watcher fixture phase={} scope_ended elapsed={:?} unwinding={}",
            self.name,
            self.started.elapsed(),
            std::thread::panicking()
        );
    }
}

pub(super) fn run<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    let _phase = Phase::new(name);
    work()
}

pub(super) async fn wait<T>(name: &'static str, timeout: Duration, work: impl Future<Output = T>) -> T {
    let _phase = Phase::new(name);
    tokio::time::timeout(timeout, work)
        .await
        .unwrap_or_else(|_| panic!("watcher fixture phase={name} exceeded {timeout:?}"))
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "watcher fixture phase=deadline-fixture exceeded")]
async fn pending_async_phase_has_a_named_deadline() {
    wait(
        "deadline-fixture",
        Duration::from_secs(1),
        std::future::pending::<()>(),
    )
    .await;
}
