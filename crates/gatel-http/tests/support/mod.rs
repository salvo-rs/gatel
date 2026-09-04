//! Shared harness: a gateway wired exactly the way the binary wires one, plus
//! a raw HTTP client so the tests depend on the wire rather than on a client
//! library agreeing with us.

#![allow(dead_code)]

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
    pub server: Arc<Server>,
    pub registry: Registry,
    pub directory: PathBuf,
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
            registry: Registry::builder()
                .bundle(gatel_bundle_std::bundle())
                .build()
                .unwrap(),
            directory,
            revision: std::cell::Cell::new(0),
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
            Err(diagnostics) => return Outcome::Rejected(diagnostics),
        };

        let binds = Arc::clone(&self.binds);
        let state = Arc::clone(&self.state);
        let certificates = Arc::clone(&self.certificates);
        let root = self.directory.clone();
        let ca_directory = self.directory.join("ca");
        let outcome = self.store.attempt(&snapshot, &self.registry, move |preparation| {
            prepare(
                &preparation,
                &Resources {
                    binds: &binds,
                    state: &state,
                    certificates: &certificates,
                    root: &root,
                    internal_ca: ca_directory.clone(),
                },
            )
        });
        if matches!(outcome, Outcome::Started { .. } | Outcome::Reloaded { .. }) {
            self.server.sync().await;
        }
        outcome
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
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub fn free_port() -> u16 {
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
