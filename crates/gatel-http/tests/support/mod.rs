//! Shared harness: a gateway wired exactly the way the binary wires one, plus
//! a raw HTTP client so the tests depend on the wire rather than on a client
//! library agreeing with us.

#![allow(dead_code)]

pub mod acme_ca;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gatel_http::{BindTable, Resources, Runtime, Server, prepare};
use gatel_model::{Snapshot, SourceId, SourceSnapshot};
use gatel_plan::{GenerationStore, Outcome, Registry};
use gatel_state::StateRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct Gateway {
    pub store: Arc<GenerationStore<Arc<Runtime>>>,
    pub binds: Arc<BindTable>,
    pub state: Arc<StateRegistry>,
    pub certificates: Arc<gatel_tls::CertificateStore>,
    pub challenges: Arc<gatel_tls::Challenges>,
    /// The account with the certificate authority, opened on first use and
    /// kept: it is the process's, not a generation's.
    pub acme: std::sync::Mutex<Option<Arc<gatel_tls::Acme>>>,
    pub telemetry: Arc<gatel_observe::Telemetry>,
    pub server: Arc<Server>,
    pub registry: Registry,
    pub directory: PathBuf,
    /// A reopened gateway shares the first one's directory and must not delete
    /// it out from under the test.
    owns_directory: bool,
    revision: std::cell::Cell<u64>,
}

impl Gateway {
    pub fn new(tag: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "gatel-e2e-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(GenerationStore::new());
        let binds = Arc::new(BindTable::new());
        Self {
            server: Arc::new(Server::new(Arc::clone(&store), Arc::clone(&binds))),
            store,
            binds,
            state: Arc::new(StateRegistry::new()),
            certificates: Arc::new(gatel_tls::CertificateStore::new()),
            challenges: gatel_tls::Challenges::new(),
            acme: std::sync::Mutex::new(None),
            telemetry: gatel_observe::Telemetry::new(),
            registry: Registry::builder()
                .bundle(gatel_bundle_std::bundle())
                .build()
                .unwrap(),
            directory,
            revision: std::cell::Cell::new(0),
            owns_directory: true,
        }
    }

    /// Stop serving and release every socket, the way a process exiting does.
    ///
    /// Needed before [`Gateway::reopen`], because two of these in one test are
    /// two processes only in the ways that matter — the sockets are real and
    /// only one of them can have a port.
    pub async fn stop(&self) {
        self.server.shutdown().await;
        self.binds.retain(&[]);
    }

    /// A second process over the same configuration directory.
    ///
    /// Everything a process owns starts empty — the certificate store, the
    /// generation, the state — and everything on disk is still there, which is
    /// what a restart is.
    pub fn reopen(previous: &Gateway) -> Self {
        let store = Arc::new(GenerationStore::new());
        let binds = Arc::new(BindTable::new());
        Self {
            server: Arc::new(Server::new(Arc::clone(&store), Arc::clone(&binds))),
            store,
            binds,
            state: Arc::new(StateRegistry::new()),
            certificates: Arc::new(gatel_tls::CertificateStore::new()),
            challenges: gatel_tls::Challenges::new(),
            acme: std::sync::Mutex::new(None),
            telemetry: gatel_observe::Telemetry::new(),
            registry: Registry::builder()
                .bundle(gatel_bundle_std::bundle())
                .build()
                .unwrap(),
            directory: previous.directory.clone(),
            revision: std::cell::Cell::new(0),
            owns_directory: false,
        }
    }

    /// Apply a configuration exactly the way the binary does.
    pub async fn apply(&self, kdl: &str) -> Outcome {
        let path = self.directory.join("gatel.kdl");
        std::fs::write(&path, kdl).unwrap();
        self.revision.set(self.revision.get() + 1);

        let source = gatel_kdl::snapshot(
            &path,
            SourceId::file("main"),
            Some(&SourceSnapshot::new(
                SourceId::file("main"),
                gatel_model::SourceRev(self.revision.get() - 1),
                gatel_model::Hasher::new().finish(),
                Vec::new(),
            )),
        );
        let snapshot = match source {
            Ok(source) => Snapshot::new().with(source),
            Err(diagnostics) => {
                let outcome = Outcome::Rejected(diagnostics);
                outcome.record(&self.telemetry);
                return outcome;
            }
        };

        let binds = Arc::clone(&self.binds);
        let state = Arc::clone(&self.state);
        let certificates = Arc::clone(&self.certificates);
        let challenges = Arc::clone(&self.challenges);
        let acme = self.acme_factory();
        let telemetry = Arc::clone(&self.telemetry);
        let root = self.directory.clone();
        let ca_directory = self.directory.join("ca");
        let outcome = self.store.attempt(&snapshot, &self.registry, move |preparation| {
            prepare(
                &preparation,
                &Resources {
                    binds: &binds,
                    state: &state,
                    certificates: &certificates,
                    challenges: &challenges,
                    acme: &acme,
                    telemetry: &telemetry,
                    root: &root,
                    internal_ca: ca_directory.clone(),
                },
            )
        });
        outcome.record(&self.telemetry);
        if matches!(outcome, Outcome::Started { .. } | Outcome::Reloaded { .. }) {
            self.server.sync().await;
            // What `App::activate` does, so readiness in a test means what it
            // means in the binary: bound addresses, not intended ones.
            self.telemetry
                .health
                .committed(self.server.active_addresses().await.len());
        }
        outcome
    }

    /// Forget the metric series the current generation no longer names, the way
    /// `App::activate` does after a commit.
    pub fn retain_series(&self) {
        let Some(committed) = self.store.current() else {
            return;
        };
        let mut live = gatel_observe::Live::default();
        for listener in &committed.plan.listeners {
            live.listener(&listener.listener.name);
            for site in &listener.sites {
                for route in &site.routes {
                    live.route(site.id.name(), route.id.name());
                }
            }
        }
        for upstream in &committed.plan.spec.upstreams {
            live.upstream(upstream.id.name());
        }
        self.telemetry.metrics.retain(&live);
    }

    /// The account with the certificate authority, opened on first use, the way
    /// `App` does it.
    fn acme_factory(&self) -> impl Fn(&gatel_plan::Acme) -> Result<Arc<gatel_tls::Acme>, String> + use<'_> {
        move |config| {
            let mut held = self.acme.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(acme) = held.as_ref() {
                return Ok(Arc::clone(acme));
            }
            let acme = gatel_tls::Acme::new(
                config,
                &self.directory,
                Arc::clone(&self.challenges),
                Arc::clone(&self.certificates),
                Arc::clone(&self.telemetry),
            )?;
            *held = Some(Arc::clone(&acme));
            Ok(acme)
        }
    }

    /// What `App::activate` does for automatic certificates.
    pub async fn reconcile_acme(&self) {
        let Some(committed) = self.store.current() else {
            return;
        };
        if let Some((acme, wanted)) = &committed.runtime.acme {
            acme.reconcile(wanted.clone()).await;
        }
    }

    pub fn address(&self) -> SocketAddr {
        *self
            .store
            .current()
            .expect("a generation must be committed before asking where it serves")
            .runtime
            .listeners
            .keys()
            .next()
            .expect("every generation in these tests has one listener")
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if self.owns_directory {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

/// A port nothing is using, and that nothing else in this test run will take.
///
/// Asking the operating system for a free port hands the same one out again a
/// moment later, and each test binary is its own process with its own memory of
/// what it claimed — so two of them racing would collide, which is exactly the
/// intermittent failure that is worth not having. Each process therefore works
/// through a window of its own, derived from its id, and still checks that each
/// port is actually free before using it.
pub fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};

    const WINDOWS: u16 = 400;
    const PER_WINDOW: u16 = 100;
    const BASE: u16 = 20_000;

    static NEXT: AtomicU16 = AtomicU16::new(0);
    let window = (std::process::id() as u16) % WINDOWS;
    let start = BASE + window * PER_WINDOW;

    for _ in 0..PER_WINDOW {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed) % PER_WINDOW;
        let port = start + offset;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    // Every port in this window is busy, which means something else is using
    // them. Fall back to asking rather than failing the test.
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A minimal HTTP/1.1 client: raw enough that the test depends on the wire,
/// not on a client library agreeing with us.
pub async fn request(address: SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect to the gateway");
    stream.write_all(raw.as_bytes()).await.unwrap();
    read_response(&mut stream).await
}

async fn read_response(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(read)) => read,
            Ok(Err(_)) => break,
        };
        buffer.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&buffer);
        // Stop once the declared body has arrived, so a keep-alive connection
        // does not make the read block until it times out.
        if let Some((head, body)) = text.split_once("\r\n\r\n")
            && let Some(length) = content_length(head)
            && body.len() >= length
        {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
}

pub async fn get(address: SocketAddr, path: &str) -> String {
    request(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"),
    )
    .await
}

/// A backend that reports which instance answered.
pub async fn backend(name: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let forwarded = head
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("x-forwarded-for:"))
                        .and_then(|line| line.split(':').nth(1))
                        .map(|value| value.trim().to_string())
                        .unwrap_or_else(|| "none".to_string());
                    let body = format!("{name} xff={forwarded}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

/// Write a self-signed pair into the gateway's configuration directory and
/// return the KDL that points at it.
pub fn manual_certificate(gateway: &Gateway, host: &str) -> String {
    let generated = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    std::fs::write(gateway.directory.join("cert.pem"), generated.cert.pem()).unwrap();
    std::fs::write(
        gateway.directory.join("key.pem"),
        generated.signing_key.serialize_pem(),
    )
    .unwrap();
    "tls cert=\"cert.pem\" key=\"key.pem\"".to_string()
}

/// A backend that accepts an upgrade and then echoes bytes back.
///
/// Deliberately not a WebSocket implementation: what the gateway must get right
/// is the handshake and the byte pipe, and a real framing library would only
/// hide whether it did.
pub async fn upgrade_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                // Read exactly the request head, so whatever follows belongs to
                // the tunnel rather than being swallowed here.
                while stream.read_exact(&mut byte).await.is_ok() {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                if !text.contains("upgrade: websocket") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                if stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\n\
                          Connection: Upgrade\r\nUpgrade: websocket\r\n\
                          Sec-WebSocket-Accept: test\r\n\r\n",
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    let echoed: Vec<u8> = b"echo:"
                        .iter()
                        .copied()
                        .chain(buffer[..read].iter().copied())
                        .collect();
                    if stream.write_all(&echoed).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}
