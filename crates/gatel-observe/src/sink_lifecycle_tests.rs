use std::future::Future;
use std::pin::Pin;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use super::*;

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
    entered: AtomicBool,
    flushed: AtomicBool,
}

struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}

struct Blocked(Arc<Gate>);
impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut released = self.0.released.lock().unwrap();
        self.0.entered.store(true, Ordering::Release);
        while !*released {
            released = self.0.changed.wait(released).unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flushed.store(true, Ordering::Release);
        Ok(())
    }
}

fn blocked(sinks: &Sinks, name: &str) -> (Arc<Sink>, Release) {
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let sink = sinks
        .resolve(name, |name| {
            spawn(
                name,
                Destination::Injected {
                    writer: Box::new(Blocked(gate)),
                    rotation_error: false,
                },
                Rotate::NEVER,
            )
        })
        .unwrap();
    sink.write("blocked inside the OS writer");
    eventually(|| release.0.entered.load(Ordering::Acquire));
    (sink, release)
}

fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < deadline, "writer did not reach expected state");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn poll(drain: &mut Pin<Box<Drain>>, wakes: &Arc<Wakes>) -> Poll<ShutdownReport> {
    drain
        .as_mut()
        .poll(&mut Context::from_waker(&Waker::from(wakes.clone())))
}

#[test]
fn last_snapshot_drop_never_waits_on_the_writer_and_reopen_waits_for_retirement() {
    let sinks = Sinks::new();
    let (sink, release) = blocked(&sinks, "last-snapshot");
    let snapshot = sinks.snapshot();
    drop(sink);
    let (sent, received) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(snapshot);
        sent.send(()).unwrap();
    });
    received
        .recv_timeout(Duration::from_secs(1))
        .expect("dropping the final metrics handle must not join I/O");
    dropper.join().unwrap();
    assert_eq!(sinks.pending_writers(), 1);
    let reopened = sinks.resolve("last-snapshot", |name| {
        spawn(name, Destination::Stderr, Rotate::NEVER)
    });
    assert!(reopened.unwrap_err().contains("still draining"));
    drop(release);
    eventually(|| sinks.pending_writers() == 0);
    assert!(
        sinks
            .resolve("last-snapshot", |name| spawn(
                name,
                Destination::Stderr,
                Rotate::NEVER
            ))
            .is_ok()
    );
}

#[test]
fn shutdown_closes_live_handles_and_cancelled_waits_leave_bounded_worker_ownership() {
    let telemetry = crate::Telemetry::new();
    let sinks = &telemetry.sinks;
    let (sink, release) = blocked(sinks, "shutdown");
    let wakes = Arc::new(Wakes::default());
    let mut drain = Box::pin(sinks.shutdown());
    assert!(poll(&mut drain, &wakes).is_pending());
    sink.write("queue already closed");
    assert_eq!(sink.dropped(), 1);
    assert!(sinks.stderr().unwrap_err().contains("shutting down"));
    // Equivalent to the host cancelling the drain future at its deadline.
    drop(drain);
    sinks.record_shutdown_timeout();
    assert_eq!(sinks.pending_writers(), 1);
    let metrics = crate::prometheus(&telemetry);
    assert!(metrics.contains("gatel_log_shutdown_timeouts_total 1\n"));
    assert!(metrics.contains("gatel_log_pending_writers 1\n"));
    let mut retry = Box::pin(sinks.shutdown());
    assert!(poll(&mut retry, &wakes).is_pending());
    let gate = release.0.clone();
    drop(release);
    eventually(|| wakes.0.load(Ordering::Relaxed) != 0);
    eventually(|| sinks.pending_writers() == 0);
    assert!(gate.flushed.load(Ordering::Acquire));
    assert_eq!(
        poll(&mut retry, &wakes),
        Poll::Ready(ShutdownReport { writers: 1 })
    );
    let metrics = crate::prometheus(&telemetry);
    assert!(metrics.contains("gatel_log_shutdown_timeouts_total 1\n"));
    assert!(metrics.contains("gatel_log_pending_writers 0\n"));
}

#[test]
fn retiring_workers_consume_the_same_budget_as_active_writers() {
    let sinks = Sinks {
        open: Arc::new(Mutex::new(Registry {
            limit: 2,
            ..Default::default()
        })),
    };
    let (first, release_first) = blocked(&sinks, "first");
    let (second, release_second) = blocked(&sinks, "second");
    drop(first);
    assert_eq!(sinks.pending_writers(), 2);
    let create_third = || sinks.resolve("third", |name| spawn(name, Destination::Stderr, Rotate::NEVER));
    assert!(create_third().unwrap_err().contains("limit (2)"));
    drop(release_first);
    eventually(|| sinks.pending_writers() == 1);
    let third = create_third().unwrap();
    assert_eq!(sinks.open.lock().unwrap().open.len(), 2);
    drop(release_second);
    drop((second, third));
    eventually(|| sinks.pending_writers() == 0);
}
