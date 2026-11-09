//! Accepting connections.
//!
//! One task per bound address, living as long as the address does rather than
//! as long as a generation. A connection pins the generation that was current
//! when it was accepted and keeps it until it closes, so a reload never changes
//! what an in-flight request sees, and an HTTP/2 connection's later streams stay
//! with the configuration their connection started under. An explicit
//! reload-drain policy gracefully retires older connections with a deadline.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use gatel_plan::{Committed, GenerationStore};
use gatel_tls::TlsAcceptor;
use hyper::service::Service as _;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
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
    blocking: gatel_plan::blocking::Executor,
    closed: std::sync::atomic::AtomicBool,
    plugin_retirements: Arc<crate::plugin_retirement::Retirements>,
    store: Arc<Store>,
    binds: Arc<BindTable>,
    tasks: tokio::sync::Mutex<BTreeMap<SocketAddr, AcceptTask>>,
    retiring: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The process-wide connection cap. One count across every listener,
    /// because file descriptors and memory are the process's, not an address's.
    admission: Arc<crate::connection_admission::Admission>,
    connection_retirements: Arc<crate::connection_drain::Retirements>,
}

impl Server {
    pub fn new(store: Arc<Store>, binds: Arc<BindTable>) -> Self {
        Self::with_blocking_executor(store, binds, gatel_plan::blocking::Executor::process_default())
    }

    /// Embedded hosts own the controller and must reuse its executor on reload.
    pub fn with_blocking_executor(
        store: Arc<Store>,
        binds: Arc<BindTable>,
        blocking: gatel_plan::blocking::Executor,
    ) -> Self {
        Self {
            blocking,
            closed: std::sync::atomic::AtomicBool::new(false),
            plugin_retirements: Arc::new(crate::plugin_retirement::Retirements::default()),
            store,
            binds,
            tasks: tokio::sync::Mutex::new(BTreeMap::new()),
            retiring: tokio::sync::Mutex::new(Vec::new()),
            admission: crate::connection_admission::Admission::new(),
            connection_retirements: Arc::new(crate::connection_drain::Retirements::default()),
        }
    }

    pub fn plugin_retirements(&self) -> &Arc<crate::plugin_retirement::Retirements> {
        &self.plugin_retirements
    }

    pub fn blocking_executor(&self) -> &gatel_plan::blocking::Executor {
        &self.blocking
    }

    /// Move the connection cap to what the committed generation asks for.
    ///
    /// Lowering it never closes anything: the connections already open keep
    /// their slots and the cap takes effect as they end, because dropping a
    /// live request to satisfy a number an operator just typed would be a
    /// worse surprise than being briefly over it.
    fn set_capacity(&self, wanted: u32) {
        self.admission.resize(wanted);
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
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        committed.runtime.plugin_tasks.activate();
        self.connection_retirements
            .retire_before(committed.number, committed.runtime.reload_drain);
        self.set_capacity(committed.runtime.limits.max_connections);
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
            let admission = Arc::clone(&self.admission);
            let retirements = Arc::clone(&self.connection_retirements);
            let handle = tokio::spawn(accept_loop(
                listener,
                address,
                store,
                admission,
                stopped,
                retirements,
            ));
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
                let mut retiring = self.retiring.lock().await;
                retiring.retain(|handle| !handle.is_finished());
                retiring.push(task.handle);
                tracing::info!(target: "gatel::listener", event = "listener_stopped", %address);
            }
        }
        drop(tasks);

        self.binds.retain(&wanted);
    }

    /// Stop accepting and wait for in-flight requests, up to the grace period.
    pub async fn shutdown(&self) {
        let mut tasks = self.tasks.lock().await;
        self.closed.store(true, std::sync::atomic::Ordering::Release);
        // Acquire both containers before moving ownership: cancellation while
        // acquiring a lock must never drop a live listener's join handle.
        let mut retiring = self.retiring.lock().await;
        for (address, task) in std::mem::take(&mut *tasks) {
            let _ = task.stop.send(true);
            retiring.push(task.handle);
            tracing::debug!(target: "gatel::listener", event = "listener_stopped", %address);
        }
        // Each accept loop owns its connections and enforces the configured
        // deadline. Signal every loop before awaiting any one of them.
        while let Some(handle) = retiring.last_mut() {
            let _ = handle.await;
            retiring.pop();
        }
        drop(retiring);
        self.plugin_retirements.shutdown().await;
    }

    pub async fn active_addresses(&self) -> Vec<SocketAddr> {
        self.tasks.lock().await.keys().copied().collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Emergency fallback. Explicit shutdown is the only graceful path.
        for task in self.tasks.get_mut().values() {
            let _ = task.stop.send(true);
            task.handle.abort();
        }
        for handle in self.retiring.get_mut().iter() {
            handle.abort();
        }
    }
}

async fn accept_loop(
    listener: Arc<TcpListener>,
    address: SocketAddr,
    store: Arc<Store>,
    admission: Arc<crate::connection_admission::Admission>,
    mut stop: watch::Receiver<bool>,
    retirements: Arc<crate::connection_drain::Retirements>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = connections.join_next(), if !connections.is_empty() => continue,
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

        // An idle listener owns no process permit. At most one accepted socket
        // per listener waits here, without a task or protocol handshake; the
        // rest remain in the backlog. Admission's FIFO queue shares available
        // capacity across the listeners that actually have traffic.
        let reservation = admission.acquire();
        tokio::pin!(reservation);
        let slot = loop {
            tokio::select! {
                biased;
                _ = stop.changed() => break None,
                _ = connections.join_next(), if !connections.is_empty() => {},
                slot = &mut reservation => break Some(slot),
            }
        };
        let Some(slot) = slot else {
            break;
        };

        // The generation is pinned here, once, and held for the whole
        // connection.
        let Some((committed, retirement)) = retirements.pin(&store) else {
            continue;
        };
        // Existing accept loops can see a new commit before Server::sync.
        // Activation is idempotent and must precede its first request too.
        committed.runtime.plugin_tasks.activate();
        let Some(runtime) = committed.runtime.listeners.get(&address).cloned() else {
            // The address is no longer served by the current generation.
            continue;
        };

        let _ = stream.set_nodelay(true);
        let stopped = stop.clone();
        connections.spawn(async move {
            // Owned by the task, including retirement and an upgraded tunnel.
            let _slot = slot;
            let serving = serve_connection(stream, peer, address, runtime, committed, stopped, &retirement);
            tokio::pin!(serving);
            tokio::select! {
                _ = &mut serving => {},
                deadline = retirement.deadline() => {
                    let _ = tokio::time::timeout_at(deadline, serving).await;
                },
            }
        });
    }

    // Include TLS/PROXY handshakes and upgrade handoffs, not only HTTP futures.
    let grace = store
        .current()
        .map(|current| current.runtime.grace_period)
        .unwrap_or(std::time::Duration::from_secs(30));
    let drain = async { while connections.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(grace, drain).await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    mut peer: SocketAddr,
    local: SocketAddr,
    runtime: Arc<crate::prepare::ListenerRuntime>,
    generation: Arc<Committed<Arc<Runtime>>>,
    stop: watch::Receiver<bool>,
    retirement: &crate::connection_drain::Retirement,
) {
    let limits = &generation.runtime.limits;
    let metrics = Arc::clone(&runtime.metrics);
    metrics.connections_total.increment();
    metrics.connections_active.increment();
    let _open = Open(Arc::clone(&metrics));

    // Before TLS and before HTTP, because that is where the bytes are.
    //
    // Only from a peer the configuration named, and a connection from anywhere
    // else is closed rather than served: anybody who can reach the port can
    // claim any address, so reading the header from a stranger would hand
    // every ip-based decision to whoever asked. The same rule already decides
    // whether a forwarding header is believed.
    if runtime.proxy_protocol {
        if !crate::dispatch::is_trusted(peer.ip(), &generation.runtime.trusted_proxies) {
            tracing::warn!(
                target: "gatel::connection",
                event = "proxy_header_refused",
                %peer,
                reason = "the peer is not a trusted-proxy",
            );
            return;
        }
        match crate::proxy_protocol::read(&mut stream, limits.handshake_timeout).await {
            Ok(crate::proxy_protocol::Claim::Client(client)) => peer = client,
            // The balancer's own connection, such as a health check. Its
            // address is the right one to attribute this to.
            Ok(crate::proxy_protocol::Claim::Local) => {}
            Err(error) => {
                tracing::warn!(
                    target: "gatel::connection",
                    event = "proxy_header_refused",
                    %peer,
                    reason = %error,
                );
                return;
            }
        }
    }
    // A guard rather than a decrement at the end: this function returns from
    // four places, and one of them being forgotten would make the gauge climb
    // forever.
    let scheme = if runtime.is_tls() {
        Scheme::HTTPS
    } else {
        Scheme::HTTP
    };
    let handler =
        runtime
            .service
            .hyper_handler(local.into(), peer.into(), scheme, None, ConnCtrl::new(), None);
    let (stream, output) = crate::socket_output::configure(stream, !runtime.is_tls());
    let work = crate::connection_work::ConnectionWork::default();
    let request_work = work.clone();
    let activity = crate::connection_activity::Activity::new();
    let request_activity = activity.clone();
    let handler = Arc::new((handler, output));
    let handler = hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
        let http1 = request.version() == hyper::Version::HTTP_11;
        request.extensions_mut().insert(request_work.clone());
        let active = request_activity.request();
        let handler = handler.clone();
        async move {
            handler.0.call(request).await.map(|response| {
                if let Some(output) = &handler.1 {
                    output.set_file(
                        http1
                            && response
                                .extensions()
                                .get::<gatel_plan::BoundedFileOutput>()
                                .is_some(),
                    );
                }
                response.map(|body| crate::connection_activity::Body::new(body, active))
            })
        }
    });
    let mut builder = auto::Builder::new(TokioExecutor::new());
    // What the head may cost, and how long a connection may go without one
    // arriving. The second covers a keep-alive connection waiting for its next
    // request and a client dribbling a head out a byte at a time, because from
    // the socket those are the same thing.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.idle_timeout)
        .max_buf_size(max_buf_size(limits.max_header.bytes()));
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_header_list_size(limits.max_header.bytes().min(u32::MAX as u64) as u32);

    // A client that opens a connection and never finishes the handshake must
    // not hold a slot: the deadline that bounds a slow request head bounds
    // this too.
    let result = match &runtime.tls {
        None => {
            let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), handler);
            crate::connection_drain::http(
                connection,
                stop,
                retirement,
                &work,
                &activity,
                limits.idle_timeout,
            )
            .await
        }
        Some(config) => {
            let acceptor = TlsAcceptor::from(Arc::clone(config));
            match tokio::time::timeout(limits.handshake_timeout, acceptor.accept(stream.into_inner())).await {
                Ok(Ok(stream)) => {
                    metrics.handshakes_ok.increment();
                    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), handler);
                    crate::connection_drain::http(
                        connection,
                        stop,
                        retirement,
                        &work,
                        &activity,
                        limits.idle_timeout,
                    )
                    .await
                }
                Ok(Err(error)) => {
                    metrics.handshakes_failed.increment();
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
                    metrics.handshakes_failed.increment();
                    tracing::debug!(
                        target: "gatel::connection",
                        event = "tls_handshake_timeout",
                        %peer,
                        after = ?limits.handshake_timeout,
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

/// Hyper reads a head into this buffer, so it bounds the head — but it also has
/// to be big enough for hyper to make progress at all.
fn max_buf_size(max_header: u64) -> usize {
    const FLOOR: u64 = 8192;
    max_header.clamp(FLOOR, usize::MAX as u64) as usize
}

/// Holds the active-connection gauge up for as long as the connection lives.
struct Open(Arc<gatel_observe::metrics::Listener>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.connections_active.decrement();
    }
}
