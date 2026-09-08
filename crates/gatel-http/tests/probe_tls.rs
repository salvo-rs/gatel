mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gatel_state::endpoint::Health;
use http_body_util::Full;
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;

struct Backend(tokio::task::JoinHandle<()>);
impl Drop for Backend {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn tls_backend(h2: bool) -> (std::net::SocketAddr, String, Backend) {
    tls_backend_with_client(h2, None).await
}

async fn tls_backend_with_client(
    h2: bool,
    client: Option<rustls::pki_types::CertificateDer<'static>>,
) -> (std::net::SocketAddr, String, Backend) {
    let certificate = rcgen::generate_simple_self_signed(vec!["api.internal".into()]).unwrap();
    let builder = rustls::ServerConfig::builder();
    let builder = if let Some(client) = &client {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(client.clone()).unwrap();
        builder.with_client_cert_verifier(
            rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .unwrap(),
        )
    } else {
        builder.with_no_client_auth()
    };
    let mut config = builder
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
        )
        .unwrap();
    config.alpn_protocols = vec![if h2 { b"h2".to_vec() } else { b"http/1.1".to_vec() }];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.unwrap();
                    let acceptor = acceptor.clone();
                    let client = client.clone();
                    peers.spawn(async move {
                        let Ok(stream) = acceptor.accept(stream).await else { return; };
                        if let Some(client) = client {
                            assert_eq!(stream.get_ref().1.peer_certificates().unwrap()[0], client);
                        }
                        let service = hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| async move {
                            assert_eq!(request.uri().path(), "/healthz");
                            if h2 {
                                assert_eq!(request.uri().scheme_str(), Some("https"));
                                assert_eq!(request.uri().authority().unwrap().as_str(), "api.internal");
                            }
                            Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(Bytes::new())))
                        });
                        if h2 {
                            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                .serve_connection(TokioIo::new(stream), service).await;
                        } else {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service).await;
                        }
                    });
                }
                result = peers.join_next(), if !peers.is_empty() => { result.unwrap().unwrap(); }
            }
        }
    });
    (address, certificate.cert.pem(), Backend(worker))
}

async fn wait_health(gateway: &Gateway, expected: Health) {
    let generation = gateway.store.current().unwrap();
    let endpoint = &generation
        .runtime
        .upstreams
        .iter()
        .next()
        .unwrap()
        .endpoints
        .all()[0];
    tokio::time::timeout(Duration::from_secs(5), async {
        while endpoint.health() != expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected:?}, got {:?}", endpoint.health()));
}

#[tokio::test]
async fn http_health_uses_tls_and_reloads_its_verification_policy() {
    for h2 in [false, true] {
        let gateway = Gateway::new("probe-tls");
        let (address, ca, _backend) = tls_backend(h2).await;
        std::fs::write(gateway.directory.join("ca.pem"), ca).unwrap();
        let unrelated = rcgen::generate_simple_self_signed(vec!["other.internal".into()]).unwrap();
        std::fs::write(gateway.directory.join("other.pem"), unrelated.cert.pem()).unwrap();
        let port = free_port();
        let protocol = if h2 { "http2" } else { "http1" };
        let supervisor = gatel_state::Supervisor::new(Arc::clone(&gateway.telemetry));
        for (name, ca, expected) in [
            ("api.internal", "ca.pem", Health::Healthy),
            ("wrong.internal", "ca.pem", Health::Unhealthy),
            ("api.internal", "ca.pem", Health::Healthy),
            ("api.internal", "other.pem", Health::Unhealthy),
            ("api.internal", "ca.pem", Health::Healthy),
        ] {
            let outcome = gateway.apply(&format!(r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{address}"
    protocol "{protocol}"
    tls server-name="{name}" ca="file:{ca}"
    health "http" path="/healthz" host="api.internal" interval="100ms" timeout="90ms" healthy-threshold=1 unhealthy-threshold=1
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#)).await;
            assert!(!outcome.is_failure(), "{outcome:?}");
            let current = gateway.store.current().unwrap();
            supervisor.reconcile(&current.runtime.upstreams).await;
            wait_health(&gateway, expected).await;
        }
        supervisor.shutdown().await;
        gateway.stop().await;
    }
}

#[tokio::test]
async fn tls_handshake_shares_the_probe_timeout_but_tcp_checks_remain_connect_only() {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        for http in [false, true] {
            let gateway = Gateway::new("probe-tls-timeout");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).await.unwrap();
                if http {
                    assert_eq!(bytes.first(), Some(&22), "TLS handshake, not plaintext HTTP");
                } else {
                    assert!(bytes.is_empty(), "TCP health must not start TLS");
                }
            });
            let port = free_port();
            let kind = if http {
                r#""http" path="/healthz""#
            } else {
                r#""tcp""#
            };
            let outcome = gateway
                .apply(&format!(
                    r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{address}"
    tls server-name="api.internal"
    health {kind} interval="60s" timeout="200ms" healthy-threshold=1 unhealthy-threshold=1
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#
                ))
                .await;
            assert!(!outcome.is_failure(), "{outcome:?}");
            let supervisor = gatel_state::Supervisor::new(Arc::clone(&gateway.telemetry));
            let current = gateway.store.current().unwrap();
            supervisor.reconcile(&current.runtime.upstreams).await;
            wait_health(&gateway, if http { Health::Unhealthy } else { Health::Healthy }).await;
            supervisor.shutdown().await;
            peer.await.unwrap();
            gateway.stop().await;
        }
    })
    .await
    .expect("TLS handshake must be bounded and its transport closed");
}

#[tokio::test]
async fn mutual_tls_health_requires_the_trusted_client_and_tracks_identity_reload() {
    for h2 in [false, true] {
        let gateway = Gateway::new("probe-mtls");
        let client = rcgen::generate_simple_self_signed(vec!["gateway.internal".into()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["untrusted.internal".into()]).unwrap();
        for (name, identity) in [("client", &client), ("other", &other)] {
            std::fs::write(gateway.directory.join(format!("{name}.pem")), identity.cert.pem()).unwrap();
            std::fs::write(
                gateway.directory.join(format!("{name}.key")),
                identity.signing_key.serialize_pem(),
            )
            .unwrap();
        }
        let (address, ca, _backend) = tls_backend_with_client(h2, Some(client.cert.der().clone())).await;
        std::fs::write(gateway.directory.join("ca.pem"), ca).unwrap();
        let port = free_port();
        let protocol = if h2 { "http2" } else { "http1" };
        let supervisor = gatel_state::Supervisor::new(Arc::clone(&gateway.telemetry));
        // Each failure starts from healthy (and vice versa), so a retained
        // verdict cannot falsely prove that the replacement worker ran.
        for (identity, expected) in [
            (None, Health::Unhealthy),
            (Some("client"), Health::Healthy),
            (Some("other"), Health::Unhealthy),
            (Some("client"), Health::Healthy),
            (None, Health::Unhealthy),
        ] {
            let identity = identity
                .map(|name| format!("client-cert=\"file:{name}.pem\" client-key=\"file:{name}.key\""))
                .unwrap_or_default();
            let outcome = gateway.apply(&format!(r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{address}"
    protocol "{protocol}"
    tls server-name="api.internal" ca="file:ca.pem" {identity}
    health "http" path="/healthz" host="api.internal" interval="1s" timeout="900ms" healthy-threshold=1 unhealthy-threshold=1
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#)).await;
            assert!(!outcome.is_failure(), "{outcome:?}");
            let current = gateway.store.current().unwrap();
            supervisor.reconcile(&current.runtime.upstreams).await;
            wait_health(&gateway, expected).await;
        }
        supervisor.shutdown().await;
        gateway.stop().await;
    }
}

#[tokio::test]
async fn prepared_reload_tracks_same_path_ca_and_client_material() {
    for refresh in [false, true] {
        for h2 in [false, true] {
            for rotate_client in [false, true] {
                let gateway = Gateway::new("probe-material-reload");
                let client = rcgen::generate_simple_self_signed(vec!["gateway.internal".into()]).unwrap();
                let other = rcgen::generate_simple_self_signed(vec!["other.internal".into()]).unwrap();
                let (address, ca, _backend) =
                    tls_backend_with_client(h2, rotate_client.then(|| client.cert.der().clone())).await;
                let port = free_port();
                let protocol = if h2 { "http2" } else { "http1" };
                let identity = if rotate_client {
                    r#"client-cert="file:client.pem" client-key="file:client.key""#
                } else {
                    ""
                };
                let supervisor = gatel_state::Supervisor::new(Arc::clone(&gateway.telemetry));
                let mut previous = None;
                for revision in 0..4 {
                    let invalid_trust = revision == 2;
                    let ca_text = if invalid_trust && !rotate_client {
                        other.cert.pem()
                    } else {
                        ca.clone()
                    };
                    // PEM whitespace is not a change to validated DER material.
                    std::fs::write(
                        gateway.directory.join("ca.pem"),
                        format!("{ca_text}{}", "\n".repeat(revision)),
                    )
                    .unwrap();
                    let identity_material = if invalid_trust { &other } else { &client };
                    std::fs::write(gateway.directory.join("client.pem"), identity_material.cert.pem())
                        .unwrap();
                    std::fs::write(
                        gateway.directory.join("client.key"),
                        identity_material.signing_key.serialize_pem(),
                    )
                    .unwrap();
                    let version = if refresh { 0 } else { revision };
                    let config = format!(
                        r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{address}"
    protocol "{protocol}"
    tls server-name="api.internal" ca="file:ca.pem" {identity}
    health "http" path="/healthz" host="api.internal" interval="1s" timeout="900ms" healthy-threshold=1 unhealthy-threshold=1
}}
site "*" {{
    route "/version" {{ respond "{version}" }}
    route "/*" {{ proxy upstream="api" }}
}}
"#
                    );
                    if refresh && revision > 0 {
                        let before = gateway.store.current().unwrap();
                        assert!(matches!(
                            gateway.apply(&config).await,
                            gatel_plan::Outcome::NoOp { .. }
                        ));
                        assert!(Arc::ptr_eq(&before, &gateway.store.current().unwrap()));
                    }
                    let outcome = if refresh {
                        gateway.refresh(&config).await
                    } else {
                        gateway.apply(&config).await
                    };
                    assert!(!outcome.is_failure(), "{outcome:?}");
                    let current = gateway.store.current().unwrap();
                    let pool = Arc::clone(&current.runtime.upstreams.iter().next().unwrap().pool);
                    if let Some(previous) = &previous {
                        assert_eq!(
                            Arc::ptr_eq(previous, &pool),
                            revision == 1,
                            "reuse must follow public material, not just paths or PEM whitespace"
                        );
                    }
                    previous = Some(pool);
                    supervisor.reconcile(&current.runtime.upstreams).await;
                    wait_health(
                        &gateway,
                        if invalid_trust {
                            Health::Unhealthy
                        } else {
                            Health::Healthy
                        },
                    )
                    .await;
                }
                supervisor.shutdown().await;
                gateway.stop().await;
            }
        }
    }
}
