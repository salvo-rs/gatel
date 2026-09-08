//! Generation-owned work: caller cancellation never owns process cleanup.
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use gatel_plugin_runtime::{BackgroundTask, Shutdown, ShutdownSignal};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::exchange;

/// A job is inert until `run` is polled. After run succeeds, fails, panics or is
/// cancelled, cleanup must terminate/reap its process scope before returning.
/// Implementations also need synchronous emergency termination in Drop for
/// host abort/runtime teardown. This trusted Rust contract is not a sandbox.
pub trait Job: Send + 'static {
    fn run(&mut self) -> impl Future<Output = Result<u64, exchange::Error>> + Send;
    fn cleanup(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

#[derive(Debug)]
pub enum Error {
    Busy,
    Closed,
    Cancelled,
    Panicked,
    Cleanup,
    Exchange(exchange::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("CGI process capacity exhausted"),
            Self::Closed => f.write_str("CGI supervisor closed"),
            Self::Cancelled => f.write_str("CGI invocation cancelled"),
            Self::Panicked => f.write_str("CGI invocation panicked"),
            Self::Cleanup => f.write_str("CGI process cleanup failed"),
            Self::Exchange(error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for Error {}

struct Request<J> {
    job: J,
    reply: oneshot::Sender<Result<u64, Error>>,
    permit: OwnedSemaphorePermit,
}
pub struct Client<J> {
    sender: mpsc::Sender<Request<J>>,
    capacity: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
}
impl<J> Clone for Client<J> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            capacity: self.capacity.clone(),
            closed: self.closed.clone(),
        }
    }
}
pub struct Supervisor<J> {
    receiver: mpsc::Receiver<Request<J>>,
    capacity: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
}

/// No tasks or processes are started here. Capacity counts queued, running,
/// and cleaning jobs together. There is no unbounded admission wait queue.
pub fn channel<J: Job>(capacity: usize) -> Result<(Client<J>, Supervisor<J>), &'static str> {
    if !(1..=1024).contains(&capacity) {
        return Err("CGI supervisor capacity must be between 1 and 1024");
    }
    channel_with_capacity(capacity, Arc::new(Semaphore::new(capacity)))
}

/// Share process permits between compatible generations, but not their queues
/// or retirement flags. The caller owns pool sizing and compatibility policy.
/// Cleanup failure poisons the shared pool; ordinary retirement does not.
pub fn channel_with_capacity<J: Job>(
    queue: usize,
    capacity: Arc<Semaphore>,
) -> Result<(Client<J>, Supervisor<J>), &'static str> {
    if !(1..=1024).contains(&queue) {
        return Err("CGI supervisor capacity must be between 1 and 1024");
    }
    let (sender, receiver) = mpsc::channel(queue);
    let closed = Arc::new(AtomicBool::new(false));
    Ok((
        Client {
            sender,
            capacity: capacity.clone(),
            closed: closed.clone(),
        },
        Supervisor {
            receiver,
            capacity,
            closed,
        },
    ))
}
impl<J: Job> Client<J> {
    /// Dropping this future cancels the exchange, not the owner's cleanup.
    pub async fn execute(&self, job: J) -> Result<u64, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => Error::Closed,
                tokio::sync::TryAcquireError::NoPermits => Error::Busy,
            })?;
        let (reply, result) = oneshot::channel();
        self.sender
            .try_send(Request { job, reply, permit })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        result.await.unwrap_or(Err(Error::Closed))
    }
}
impl<J: Job> Supervisor<J> {
    pub fn background(self) -> BackgroundTask {
        BackgroundTask::cooperative("cgi-supervisor", move |shutdown| self.run(shutdown))
    }

    pub async fn run(mut self, mut shutdown: Shutdown) -> Result<(), String> {
        let (stop, cancelled) = ShutdownSignal::new();
        let mut jobs = FuturesUnordered::new();
        let mut failed = false;
        loop {
            tokio::select! {
                biased;
                () = shutdown.requested() => break,
                result = jobs.next(), if !jobs.is_empty() => {
                    if result == Some(false) { failed = true; break; }
                }
                request = self.receiver.recv() => match request {
                    Some(request) if !request.reply.is_closed() => {
                        jobs.push(owned(request, cancelled.clone(), self.capacity.clone()));
                    }
                    Some(_) => (), // Cancelled while queued: it has never started.
                    None => break,
                },
            }
        }
        self.closed.store(true, Ordering::Release);
        self.receiver.close();
        while let Ok(request) = self.receiver.try_recv() {
            let _ = request.reply.send(Err(Error::Closed));
        }
        stop.request();
        while let Some(clean) = jobs.next().await {
            failed |= !clean;
        }
        if failed {
            Err("CGI process cleanup failed; generation sealed".into())
        } else {
            Ok(())
        }
    }
}
impl<J> Drop for Supervisor<J> {
    fn drop(&mut self) {
        // Covers unactivated candidates and forced host abort as well.
        self.closed.store(true, Ordering::Release);
    }
}

async fn owned<J: Job>(request: Request<J>, mut shutdown: Shutdown, capacity: Arc<Semaphore>) -> bool {
    let Request {
        mut job,
        mut reply,
        permit,
    } = request;
    struct ReapGuard {
        capacity: Arc<Semaphore>,
        verified: bool,
    }
    impl Drop for ReapGuard {
        fn drop(&mut self) {
            // A forced owner abort cannot prove cleanup. Poison before the
            // owned permit drops, including across compatible generations.
            if !self.verified {
                self.capacity.close();
            }
        }
    }
    let mut guard = ReapGuard {
        capacity: capacity.clone(),
        verified: false,
    };
    let result = tokio::select! {
        biased;
        () = shutdown.requested() => Err(Error::Cancelled),
        () = reply.closed() => Err(Error::Cancelled),
        result = AssertUnwindSafe(async { job.run().await }).catch_unwind() => match result {
            Ok(result) => result.map_err(Error::Exchange),
            Err(_) => Err(Error::Panicked),
        },
    };
    // No caller-cancellation or shutdown select around cleanup. The host owns
    // the overall cooperative grace period and emergency abort policy.
    let clean = matches!(
        AssertUnwindSafe(async { job.cleanup().await })
            .catch_unwind()
            .await,
        Ok(Ok(()))
    );
    guard.verified = clean;
    if !clean {
        // Never admit replacement work after an unverified reap, even in the
        // scheduling window before the supervisor observes this completion.
        capacity.close();
    }
    drop(job);
    drop(permit);
    let _ = reply.send(if clean { result } else { Err(Error::Cleanup) });
    clean
}
