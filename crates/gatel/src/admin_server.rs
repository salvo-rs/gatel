//! Explicit loopback-only HTTP/1 listener with owned connection cancellation.
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::AdminApi;

const MAX_CONNECTIONS: usize = 32;

pub(crate) struct AdminServer {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
    #[cfg(test)]
    slots: Arc<Semaphore>,
}

impl AdminServer {
    /// The caller owns explicit bind policy. Non-loopback plaintext sockets are
    /// refused even if the host accidentally hands one to this adapter.
    pub fn start(listener: std::net::TcpListener, api: Arc<AdminApi>) -> io::Result<Self> {
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plaintext admin listener must be loopback",
            ));
        }
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let (stop, receive) = oneshot::channel();
        let task = tokio::spawn(run(listener, api, receive, slots.clone()));
        Ok(Self {
            address,
            stop: Some(stop),
            task: Some(task),
            #[cfg(test)]
            slots,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[cfg(test)]
    pub fn active_connections(&self) -> usize {
        MAX_CONNECTIONS - self.slots.available_permits()
    }

    /// Closes acceptance and cancels/joins network requests, including incomplete
    /// headers/bodies. Already accepted App writes are separately owned by App;
    /// the host must subsequently call App::shutdown to drain those transactions.
    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.terminated().await
    }

    /// Cancellation-safe observation of listener termination; consumes the
    /// completed result once so shutdown never polls a finished JoinHandle again.
    /// A caller that receives an error must retain it: subsequent draining returns
    /// Ok once the result was consumed, rather than replaying the failure receipt.
    pub async fn terminated(&mut self) -> io::Result<()> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.task.take();
        match result {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("admin listener task failed")),
        }
    }
}

impl Drop for AdminServer {
    fn drop(&mut self) {
        // Best-effort asynchronous cleanup; only shutdown() proves it completed.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn run(
    listener: tokio::net::TcpListener,
    api: Arc<AdminApi>,
    mut stop: oneshot::Receiver<()>,
    slots: Arc<Semaphore>,
) -> io::Result<()> {
    let (cancel, cancelled) = watch::channel(false);
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut stop => break Ok(()),
            completed = connections.join_next(), if !connections.is_empty() => {
                if completed.is_some_and(|result| result.is_err()) {
                    tracing::warn!(target: "gatel::admin", "admin connection task failed");
                }
            }
            accepted = listener.accept() => {
                let (socket, _) = match accepted { Ok(socket) => socket, Err(error) => break Err(error) };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    // Do not start another task merely to send an overload reply.
                    drop(socket);
                    continue;
                };
                let api = api.clone();
                let mut cancelled = cancelled.clone();
                connections.spawn(async move {
                    let _slot = slot;
                    let service = hyper::service::service_fn(move |request| {
                        let api = api.clone();
                        async move { Ok::<_, Infallible>(api.handle(request).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder.keep_alive(false).max_headers(32).max_buf_size(16 * 1024)
                        .timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5));
                    let connection = builder.serve_connection(TokioIo::new(socket), service);
                    tokio::select! {
                        biased;
                        _ = cancelled.changed() => {}
                        _ = connection => {}
                    }
                });
            }
        }
    };
    drop(listener);
    let _ = cancel.send(true);
    while connections.join_next().await.is_some() {}
    result
}

#[cfg(test)]
#[path = "admin_server_tests.rs"]
mod tests;
