//! A bounded writer registry owns slow I/O; destructors only close queues.
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

pub(super) type Queue = Arc<Mutex<Option<SyncSender<Vec<u8>>>>>;

#[derive(Debug, Default)]
pub(super) struct Completion {
    done: AtomicBool,
    waiters: Mutex<Vec<Weak<Wake>>>,
}

impl Completion {
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn register(&self, wake: &Arc<Wake>) {
        let mut waiters = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        waiters.retain(|waiter| waiter.strong_count() != 0);
        waiters.push(Arc::downgrade(wake));
    }
}

/// Drop only after the destination (including its final file close) is gone.
pub(super) struct Completed(pub Arc<Completion>);

impl Drop for Completed {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        let waiters = std::mem::take(&mut *self.0.waiters.lock().unwrap_or_else(|p| p.into_inner()));
        for waiter in waiters.into_iter().filter_map(|waiter| waiter.upgrade()) {
            let waker = waiter.0.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct Worker {
    // Kept by the registry until destination I/O and closing have completed.
    // Dropping a JoinHandle detaches; it never waits on the caller's thread.
    _handle: std::thread::JoinHandle<()>,
    pub completion: Arc<Completion>,
    queue: Queue,
}

impl Worker {
    pub fn new(handle: std::thread::JoinHandle<()>, completion: Arc<Completion>, queue: Queue) -> Self {
        Self {
            _handle: handle,
            completion,
            queue,
        }
    }

    pub fn close(&self) {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).take();
    }

    pub fn finished(&self) -> bool {
        self.completion.finished()
    }
}

#[derive(Debug, Default)]
struct Wake(Mutex<Option<Waker>>);

/// Successful completion of the writers present when shutdown started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    pub writers: usize,
}

/// Runtime-independent drain future. The host supplies its own timeout.
///
/// Cancelling this wait does not kill blocked OS I/O: the registry still owns
/// those workers, and each worker owns its destination until the call returns.
pub struct Drain {
    workers: Vec<Arc<Worker>>,
    wake: Arc<Wake>,
    registered: bool,
}

impl Drain {
    pub(super) fn new(workers: Vec<Arc<Worker>>) -> Self {
        Self {
            workers,
            wake: Arc::default(),
            registered: false,
        }
    }
}

impl Future for Drain {
    type Output = ShutdownReport;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        *self.wake.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx.waker().clone());
        if !self.registered {
            for worker in &self.workers {
                worker.completion.register(&self.wake);
            }
            self.registered = true;
        }
        // Recheck after registration so a finishing worker cannot lose a wake.
        if self.workers.iter().all(|worker| worker.finished()) {
            Poll::Ready(ShutdownReport {
                writers: self.workers.len(),
            })
        } else {
            Poll::Pending
        }
    }
}
