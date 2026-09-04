//! The whole stack over a real socket: KDL text in, HTTP responses out.
//!
//! These are the tests that would notice if any layer stopped agreeing with the
//! others — the compiler's route order, the dispatcher's matching, the pool's
//! reuse, and the transaction's promise that a bad reload changes nothing.

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

struct Gateway {
    store: Arc<GenerationStore<Arc<Runtime>>>,
    binds: Arc<BindTable>,
    state: Arc<StateRegistry>,
    certificates: Arc<gatel_tls::CertificateStore>,
    server: Arc<Server>,
    registry: Registry,
    directory: PathBuf,
    revision: std::cell::Cell<u64>,
}

impl Gateway {
    fn new(tag: &str) -> Self {
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
    async fn apply(&self, kdl: &str) -> Outcome {
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

    fn address(&self) -> SocketAddr {
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A minimal HTTP/1.1 client: raw enough that the test depends on the wire,
/// not on a client library agreeing with us.
async fn request(address: SocketAddr, raw: &str) -> String {
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

async fn get(address: SocketAddr, path: &str) -> String {
    request(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"),
    )
    .await
}

/// A backend that reports which instance answered.
async fn backend(name: &'static str) -> SocketAddr {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn it_serves_the_route_the_compiler_chose() {
    let gateway = Gateway::new("serve");
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/health" {{
        method "GET"
        respond "ok"
    }}
    route "/*" {{ respond "fallback" }}
}}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");

    let address = gateway.address();
    assert!(get(address, "/health").await.contains("ok"));
    assert!(get(address, "/anything").await.contains("fallback"));
}

#[tokio::test]
async fn an_unmatched_request_gets_gatels_own_not_found() {
    let gateway = Gateway::new("notfound");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "example.com" {{
    route "/only" {{ respond "here" }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/elsewhere").await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("no route matched"));

    let other_host = request(
        gateway.address(),
        "GET /only HTTP/1.1\r\nHost: other.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(other_host.contains("no site is configured"), "{other_host}");
}

#[tokio::test]
async fn middleware_runs_in_phase_order_not_declaration_order() {
    let gateway = Gateway::new("phases");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    headers {{ response {{ set "x-site" "outer" }} }}
    route "/" {{
        respond "body"
        headers {{ response {{ add "x-route" "inner" }} }}
    }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(response.contains("x-site: outer"), "{response}");
    assert!(response.contains("x-route: inner"), "{response}");
    assert!(response.contains("body"));
}

#[tokio::test]
async fn a_reload_changes_behaviour_without_rebinding_the_socket() {
    let gateway = Gateway::new("reload");
    let port = free_port();
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/" {{ respond "{body}" }} }}
"#
        )
    };

    gateway.apply(&config("before")).await;
    let address = gateway.address();
    let socket = gateway.binds.get(&address).expect("bound");
    assert!(get(address, "/").await.contains("before"));

    let outcome = gateway.apply(&config("after")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    assert!(get(address, "/").await.contains("after"));
    assert!(
        Arc::ptr_eq(&socket, &gateway.binds.get(&address).unwrap()),
        "a reload must not close and reopen the listening socket"
    );
}

#[tokio::test]
async fn a_cosmetic_edit_does_not_produce_a_generation() {
    let gateway = Gateway::new("noop");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"same\" }} }}\n"
        ))
        .await;

    let outcome = gateway
        .apply(&format!(
            "// a comment\nglobal {{\n    http \"127.0.0.1:{port}\"\n}}\n\nsite \"*\" {{\n    route \"/\" {{ respond \"same\" }}\n}}\n"
        ))
        .await;
    assert!(matches!(outcome, Outcome::NoOp { .. }), "{outcome:?}");
    assert_eq!(gateway.store.status().generation.number(), 1);
}

#[tokio::test]
async fn a_rejected_reload_keeps_serving_the_last_good_configuration() {
    let gateway = Gateway::new("lkg");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"good\" }} }}\n"
        ))
        .await;
    let address = gateway.address();

    let outcome = gateway.apply("global { nonsense }\n").await;
    assert!(matches!(outcome, Outcome::Rejected(_)), "{outcome:?}");
    assert!(
        get(address, "/").await.contains("good"),
        "the previous generation must still serve"
    );

    let status = gateway.store.status();
    assert!(status.is_serving());
    assert!(
        status.is_degraded(),
        "the operator must be able to see that the newest attempt failed"
    );

    // And recovery clears it.
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"recovered\" }} }}\n"
        ))
        .await;
    assert!(get(address, "/").await.contains("recovered"));
    assert!(!gateway.store.status().is_degraded());
}

#[tokio::test]
async fn a_port_conflict_fails_the_reload_and_changes_nothing() {
    let gateway = Gateway::new("conflict");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"first\" }} }}\n"
        ))
        .await;
    let address = gateway.address();

    // Take a port, then ask for it.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = taken.local_addr().unwrap().port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{
    http "a" "127.0.0.1:{port}"
    http "b" "127.0.0.1:{occupied}"
}}
site "*" {{ route "/" {{ respond "second" }} }}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert_eq!(gateway.store.status().generation.number(), 1);
    assert!(get(address, "/").await.contains("first"));
}

#[tokio::test]
async fn adding_and_removing_a_listener_works_without_a_restart() {
    let gateway = Gateway::new("listeners");
    let first = free_port();
    let second = free_port();

    gateway
        .apply(&format!(
            "global {{ http \"a\" \"127.0.0.1:{first}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"one\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 1);

    gateway
        .apply(&format!(
            r#"
global {{
    http "a" "127.0.0.1:{first}"
    http "b" "127.0.0.1:{second}"
}}
site "*" {{ route "/" {{ respond "two" }} }}
"#
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 2);
    let second_address: SocketAddr = format!("127.0.0.1:{second}").parse().unwrap();
    assert!(get(second_address, "/").await.contains("two"));

    gateway
        .apply(&format!(
            "global {{ http \"a\" \"127.0.0.1:{first}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"one again\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 1);
    assert!(
        gateway.binds.get(&second_address).is_none(),
        "the removed socket must be closed"
    );
}

#[tokio::test]
async fn it_proxies_and_balances_across_backends() {
    let gateway = Gateway::new("proxy");
    let port = free_port();
    let a = backend("alpha").await;
    let b = backend("beta").await;

    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}"
    trusted-proxy "127.0.0.0/8"
}}
upstream "api" {{
    endpoint "{a}" id="a"
    endpoint "{b}" id="b"
    balance "round_robin"
}}
site "*" {{
    route "/api/*" {{ proxy upstream="api" }}
    route "/inline" {{ proxy "{a}" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let answers: Vec<String> = {
        let mut answers = Vec::new();
        for _ in 0..4 {
            answers.push(get(address, "/api/x").await);
        }
        answers
    };
    assert_eq!(
        answers.iter().filter(|body| body.contains("alpha")).count(),
        2,
        "{answers:?}"
    );
    assert_eq!(answers.iter().filter(|body| body.contains("beta")).count(), 2);

    assert!(
        get(address, "/inline").await.contains("alpha"),
        "the short form must proxy too"
    );
}

#[tokio::test]
async fn the_client_address_reaches_the_backend() {
    let gateway = Gateway::new("forwarded");
    let port = free_port();
    let upstream = backend("only").await;

    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}"
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{ route "/*" {{ proxy "{upstream}" }} }}
"#
        ))
        .await;

    let direct = get(gateway.address(), "/").await;
    assert!(direct.contains("xff=127.0.0.1"), "{direct}");

    // The loopback peer is trusted here, so its header is believed.
    let forwarded = request(
        gateway.address(),
        "GET / HTTP/1.1\r\nHost: example.com\r\nX-Forwarded-For: 203.0.113.9\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(forwarded.contains("xff=203.0.113.9"), "{forwarded}");
}

#[tokio::test]
async fn an_unreachable_backend_produces_a_gateway_error_not_a_hang() {
    let gateway = Gateway::new("unreachable");
    let port = free_port();
    // A port nothing is listening on.
    let dead = format!("127.0.0.1:{}", free_port());

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "dead" {{
    endpoint "{dead}"
    timeouts connect="300ms" queue="200ms" response-head="1s" overall="5s"
}}
site "*" {{ route "/*" {{ proxy upstream="dead" }} }}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    // The backend address must not appear in what the client is told.
    assert!(!response.contains(&dead), "{response}");
}

#[tokio::test]
async fn connections_are_reused_across_requests() {
    let gateway = Gateway::new("reuse");
    let port = free_port();
    let upstream = backend("pooled").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ proxy "{upstream}" }} }}
"#
        ))
        .await;

    let address = gateway.address();
    for _ in 0..3 {
        assert!(get(address, "/").await.contains("pooled"));
    }

    let committed = gateway.store.current().unwrap();
    let stats = committed.runtime.upstreams.iter().next().unwrap().pool.stats();
    assert_eq!(
        stats[0].opened, 1,
        "three requests should have shared one upstream connection"
    );
    assert_eq!(stats[0].reused, 2);
}

#[tokio::test]
async fn a_compatible_reload_keeps_upstream_connections_open() {
    let gateway = Gateway::new("keep-pool");
    let port = free_port();
    let upstream = backend("kept").await;
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{ endpoint "{upstream}" }}
site "*" {{
    route "/" {{ respond "{body}" }}
    route "/api" {{ proxy upstream="api" }}
}}
"#
        )
    };

    gateway.apply(&config("before")).await;
    let address = gateway.address();
    get(address, "/api").await;
    let pool_before = Arc::clone(
        &gateway
            .store
            .current()
            .unwrap()
            .runtime
            .upstreams
            .iter()
            .next()
            .unwrap()
            .pool,
    );

    // Only an unrelated route changed, so the upstream's state must carry over.
    gateway.apply(&config("after")).await;
    let pool_after = Arc::clone(
        &gateway
            .store
            .current()
            .unwrap()
            .runtime
            .upstreams
            .iter()
            .next()
            .unwrap()
            .pool,
    );
    assert!(
        Arc::ptr_eq(&pool_before, &pool_after),
        "an unrelated edit must not drop connections"
    );

    get(address, "/api").await;
    assert_eq!(
        pool_after.stats()[0].opened,
        1,
        "the connection survived the reload"
    );
}

#[tokio::test]
async fn a_get_route_also_answers_head() {
    let gateway = Gateway::new("head");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/" {{
        method "GET"
        respond "body text"
    }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "HEAD / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        !response.contains("body text"),
        "a HEAD response carries no body: {response}"
    );
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// Write a self-signed pair into the gateway's configuration directory and
/// return the KDL that points at it.
fn manual_certificate(gateway: &Gateway, host: &str) -> String {
    let generated = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    std::fs::write(gateway.directory.join("cert.pem"), generated.cert.pem()).unwrap();
    std::fs::write(
        gateway.directory.join("key.pem"),
        generated.signing_key.serialize_pem(),
    )
    .unwrap();
    "tls cert=\"cert.pem\" key=\"key.pem\"".to_string()
}

#[tokio::test]
async fn a_manual_certificate_serves_https() {
    let gateway = Gateway::new("tls-manual");
    let port = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ https "127.0.0.1:{port}" }}
site "secure.example.com" {{
    {tls}
    route "/" {{ respond "over tls" }}
}}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");

    let committed = gateway.store.current().unwrap();
    let listener = committed.runtime.listeners.values().next().unwrap();
    assert!(listener.is_tls(), "an https listener must terminate TLS");
    // Offering h2 first is what lets a modern client skip a round trip.
    let config = listener.tls.as_ref().unwrap();
    assert_eq!(config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
}

#[tokio::test]
async fn a_plaintext_request_to_a_tls_port_is_refused_not_answered() {
    let gateway = Gateway::new("tls-plaintext");
    let port = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");
    gateway
        .apply(&format!(
            r#"
global {{ https "127.0.0.1:{port}" }}
site "secure.example.com" {{
    {tls}
    route "/" {{ respond "over tls" }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    // rustls answers with a fatal alert record (0x15) rather than parsing the
    // bytes as a request. What matters is that it is not an HTTP response.
    assert!(
        !response.starts_with("HTTP/"),
        "a TLS port must not answer plaintext: {response:?}"
    );
    assert!(
        response.is_empty() || response.as_bytes()[0] == 0x15,
        "expected a TLS alert, got {response:?}"
    );
}

#[tokio::test]
async fn a_certificate_that_will_not_load_stops_the_reload() {
    let gateway = Gateway::new("tls-bad");
    let port = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");
    gateway
        .apply(&format!(
            "global {{ https \"127.0.0.1:{port}\" }}\nsite \"secure.example.com\" {{ {tls}\n route \"/\" {{ respond \"good\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.store.status().generation.number(), 1);

    // Replace the key with one from a different certificate.
    let other = rcgen::generate_simple_self_signed(vec!["other.example.com".to_string()]).unwrap();
    std::fs::write(
        gateway.directory.join("key.pem"),
        other.signing_key.serialize_pem(),
    )
    .unwrap();

    let outcome = gateway
        .apply(&format!(
            "global {{ https \"127.0.0.1:{port}\" }}\nsite \"secure.example.com\" {{ {tls}\n route \"/\" {{ respond \"changed\" }} }}\n"
        ))
        .await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert_eq!(
        gateway.store.status().generation.number(),
        1,
        "the previous generation still serves"
    );
}

#[tokio::test]
async fn the_development_authority_issues_for_a_named_site() {
    let gateway = Gateway::new("tls-internal");
    let port = free_port();

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ https "127.0.0.1:{port}" }}
site "app.localhost" {{
    tls internal
    route "/" {{ respond "issued locally" }}
}}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
    assert_eq!(gateway.certificates.len(), 1);
    assert!(
        gateway.directory.join("ca").join("root.pem").exists(),
        "the root must be on disk to trust"
    );
}

#[tokio::test]
async fn a_site_on_a_tls_listener_without_a_certificate_is_rejected() {
    let gateway = Gateway::new("tls-nocert");
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            "global {{ https \"127.0.0.1:{port}\" }}\nsite \"secure.example.com\" {{ route \"/\" {{ respond \"x\" }} }}\n"
        ))
        .await;
    // The compiler catches this before preparation: a site with no `tls` is
    // not served on an https listener at all, so it has no listener.
    assert!(matches!(outcome, Outcome::Rejected(_)), "{outcome:?}");
}

#[tokio::test]
async fn plaintext_and_tls_listeners_coexist() {
    let gateway = Gateway::new("tls-both");
    let plain = free_port();
    let secure = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");

    gateway
        .apply(&format!(
            r#"
global {{
    http "plain" "127.0.0.1:{plain}"
    https "secure" "127.0.0.1:{secure}"
}}
site "secure.example.com" {{
    {tls}
    route "/" {{ respond "secure" }}
}}
site "*" {{
    route "/" {{ respond "plain" }}
}}
"#
        ))
        .await;

    let committed = gateway.store.current().unwrap();
    assert_eq!(committed.runtime.listeners.len(), 2);
    let plain_address: SocketAddr = format!("127.0.0.1:{plain}").parse().unwrap();
    assert!(get(plain_address, "/").await.contains("plain"));

    let secure_address: SocketAddr = format!("127.0.0.1:{secure}").parse().unwrap();
    assert!(
        committed.runtime.listeners.get(&secure_address).unwrap().is_tls(),
        "only the https listener terminates TLS"
    );
}

// ---------------------------------------------------------------------------
// Guards and directive state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rate_limit_refuses_once_the_allowance_is_spent() {
    let gateway = Gateway::new("rate-limit");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max=2 window="1h"
        respond "allowed"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(get(address, "/").await.contains("allowed"));
    assert!(get(address, "/").await.contains("allowed"));

    let refused = get(address, "/").await;
    assert!(refused.starts_with("HTTP/1.1 429"), "{refused}");
    assert!(
        refused.contains("retry-after:"),
        "a client must be told when to come back: {refused}"
    );
    assert!(refused.contains("x-ratelimit-remaining: 0"));
}

#[tokio::test]
async fn raising_a_rate_limit_keeps_what_clients_have_already_spent() {
    let gateway = Gateway::new("rate-limit-reload");
    let port = free_port();
    let config = |max: u32| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max={max} window="1h"
        respond "allowed"
    }}
}}
"#
        )
    };

    gateway.apply(&config(2)).await;
    let address = gateway.address();
    get(address, "/").await;
    get(address, "/").await;
    assert!(get(address, "/").await.starts_with("HTTP/1.1 429"));

    // A bigger allowance is still the same client being counted. The bucket
    // carries over at zero and refills toward the new ceiling, so this client
    // stays throttled — a reload does not hand everyone a fresh allowance.
    gateway.apply(&config(3)).await;
    assert!(
        get(address, "/").await.starts_with("HTTP/1.1 429"),
        "reloading must not reset what a client has already spent"
    );
    assert_eq!(
        gateway.store.status().generation.number(),
        2,
        "the reload did happen"
    );
}

#[tokio::test]
async fn changing_what_is_counted_starts_a_new_count() {
    let gateway = Gateway::new("rate-limit-rekey");
    let port = free_port();
    let config = |key: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max=1 window="1h" key="{key}"
        respond "allowed"
    }}
}}
"#
        )
    };

    gateway.apply(&config("ip")).await;
    let address = gateway.address();
    assert!(get(address, "/").await.contains("allowed"));
    assert!(get(address, "/").await.starts_with("HTTP/1.1 429"));

    // Counting per route rather than per client is a different question, so
    // the old answer does not carry over.
    gateway.apply(&config("route")).await;
    assert!(
        get(address, "/").await.contains("allowed"),
        "a different key means a fresh count"
    );
}

#[tokio::test]
async fn an_ip_filter_refuses_an_address_outside_its_allow_list() {
    let gateway = Gateway::new("ip-filter");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/open" {{ respond "open" }}
    route "/closed" {{
        ip-filter allow="10.0.0.0/8"
        respond "secret"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(get(address, "/open").await.contains("open"));

    // The test client is on loopback, which the allow list does not name.
    let refused = get(address, "/closed").await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(
        !refused.contains("10.0.0.0"),
        "the filter's shape is not the client's business"
    );
}

#[tokio::test]
async fn a_guard_runs_before_the_terminal_whatever_the_file_says() {
    let gateway = Gateway::new("guard-order");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        respond "should never be reached"
        ip-filter deny="0.0.0.0/0"
    }}
}}
"#
        ))
        .await;

    // The filter is written after the terminal; its phase puts it first.
    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(!response.contains("should never be reached"));
}

#[tokio::test]
async fn a_request_id_is_generated_and_returned() {
    let gateway = Gateway::new("request-id");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    request-id
    route "/*" {{ respond "ok" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let first = get(address, "/").await;
    assert!(first.to_lowercase().contains("x-request-id:"), "{first}");

    let second = get(address, "/").await;
    let id = |response: &str| {
        response
            .lines()
            .find(|line| line.to_lowercase().starts_with("x-request-id:"))
            .map(|line| line.to_string())
            .unwrap_or_default()
    };
    assert_ne!(id(&first), id(&second), "two requests must not share an id");
}

#[tokio::test]
async fn a_client_supplied_request_id_is_ignored_by_default() {
    let gateway = Gateway::new("request-id-untrusted");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    request-id
    route "/*" {{ respond "ok" }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "GET / HTTP/1.1\r\nHost: example.com\r\nX-Request-Id: chosen-by-the-client\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        !response.contains("chosen-by-the-client"),
        "believing a client's id lets one client's traffic be attributed to another: {response}"
    );
}
