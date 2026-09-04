//! Accepting connections.
//!
//! One task per bound address, living as long as the address does rather than
//! as long as a generation. A connection pins the generation that was current
//! when it was accepted and keeps it until it closes, so a reload never changes
//! what an in-flight request sees, and an HTTP/2 connection's later streams stay
//! with the configuration their connection started under.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use gatel_plan::{Committed, GenerationStore};
use gatel_tls::TlsAcceptor;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use salvo_core::ConnCtrl;
use salvo_core::http::uri::Scheme;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::bind::BindTable;
use crate::prepare::Runtime;

pub type Store = GenerationStore<Arc<Runtime>>;

struct AcceptTask {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

/// Owns the accept loops and keeps them in step with the current generation.
pub struct Server {
    store: Arc<Store>,
    binds: Arc<BindTable>,
    tasks: tokio::sync::Mutex<BTreeMap<SocketAddr, AcceptTask>>,
}

impl Server {
    pub fn new(store: Arc<Store>, binds: Arc<BindTable>) -> Self {
        Self {
            store,
            binds,
            tasks: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Start serving every address the current generation wants, and stop
    /// serving the ones it does not.
    ///
    /// Called after a commit, never before: an address must not start accepting
    /// until there is a generation that knows what to do with it.
    pub async fn sync(&self) {
        let Some(committed) = self.store.current() else {
            return;
        };
        let wanted = committed.runtime.addresses();

        let mut tasks = self.tasks.lock().await;
        for address in &wanted {
            if tasks.contains_key(address) {
                continue;
            }
            let Some(listener) = self.binds.get(address) else {
                continue;
            };
            let (stop, stopped) = watch::channel(false);
            let store = Arc::clone(&self.store);
            let address = *address;
            let handle = tokio::spawn(accept_loop(listener, address, store, stopped));
            tasks.insert(address, AcceptTask { stop, handle });
            tracing::info!(target: "gatel::listener", event = "listener_started", %address);
        }

        let obsolete: Vec<SocketAddr> = tasks
            .keys()
            .filter(|address| !wanted.contains(address))
            .copied()
            .collect();
        for address in obsolete {
            if let Some(task) = tasks.remove(&address) {
                let _ = task.stop.send(true);
                task.handle.abort();
                tracing::info!(target: "gatel::listener", event = "listener_stopped", %address);
            }
        }
        drop(tasks);

        self.binds.retain(&wanted);
    }

    /// Stop accepting and wait for in-flight requests, up to the grace period.
    pub async fn shutdown(&self) {
        let mut tasks = self.tasks.lock().await;
        for (address, task) in std::mem::take(&mut *tasks) {
            let _ = task.stop.send(true);
            task.handle.abort();
            tracing::debug!(target: "gatel::listener", event = "listener_stopped", %address);
        }
    }

    pub async fn active_addresses(&self) -> Vec<SocketAddr> {
        self.tasks.lock().await.keys().copied().collect()
    }
}

async fn accept_loop(
    listener: Arc<TcpListener>,
    address: SocketAddr,
    store: Arc<Store>,
    mut stop: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // A per-connection failure must not take the listener down;
                // running out of file descriptors is temporary.
                tracing::warn!(target: "gatel::listener", event = "accept_failed", %address, %error);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };

        // The generation is pinned here, once, and held for the whole
        // connection.
        let Some(committed) = store.current() else {
            continue;
        };
        let Some(runtime) = committed.runtime.listeners.get(&address).cloned() else {
            // The address is no longer served by the current generation.
            continue;
        };

        let _ = stream.set_nodelay(true);
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            serve_connection(stream, peer, address, runtime, committed, watcher).await;
        });
    }

    // Give connections that are already open a chance to finish.
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    runtime: Arc<crate::prepare::ListenerRuntime>,
    generation: Arc<Committed<Arc<Runtime>>>,
    watcher: hyper_util::server::graceful::Watcher,
) {
    let handshake_deadline = generation.runtime.limits.header_timeout;
    let scheme = if runtime.is_tls() {
        Scheme::HTTPS
    } else {
        Scheme::HTTP
    };
    let handler =
        runtime
            .service
            .hyper_handler(local.into(), peer.into(), scheme, None, ConnCtrl::new(), None);
    let builder = auto::Builder::new(TokioExecutor::new());

    // A client that opens a connection and never finishes the handshake must
    // not hold a slot: the deadline that bounds a slow request head bounds
    // this too.
    let result = match &runtime.tls {
        None => {
            let connection =
                watcher.watch(builder.serve_connection_with_upgrades(TokioIo::new(stream), handler));
            connection.await.map_err(|error| error.to_string())
        }
        Some(config) => {
            let acceptor = TlsAcceptor::from(Arc::clone(config));
            match tokio::time::timeout(handshake_deadline, acceptor.accept(stream)).await {
                Ok(Ok(stream)) => {
                    let connection =
                        watcher.watch(builder.serve_connection_with_upgrades(TokioIo::new(stream), handler));
                    connection.await.map_err(|error| error.to_string())
                }
                Ok(Err(error)) => {
                    // A failed handshake is ordinary on a public port — a
                    // scanner, a wrong name, an old client — so it is not a
                    // warning, but it is worth being able to see.
                    tracing::debug!(
                        target: "gatel::connection",
                        event = "tls_handshake_failed",
                        %peer,
                        %error,
                    );
                    return;
                }
                Err(_) => {
                    tracing::debug!(
                        target: "gatel::connection",
                        event = "tls_handshake_timeout",
                        %peer,
                        after = ?handshake_deadline,
                    );
                    return;
                }
            }
        }
    };

    if let Err(error) = result {
        tracing::debug!(target: "gatel::connection", event = "connection_closed", %peer, %error);
    }
}
