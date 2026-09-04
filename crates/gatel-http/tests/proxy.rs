//! Reverse proxying: balancing, forwarding and connection reuse.

mod support;

use std::sync::Arc;

use support::*;

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
    balance "round-robin"
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

// ---------------------------------------------------------------------------
// TLS to a backend
// ---------------------------------------------------------------------------

/// A backend that only speaks TLS, signed by an authority the test controls.
async fn tls_backend(name: &str) -> (std::net::SocketAddr, String) {
    use std::sync::Arc;

    let authority = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    let chain = vec![rustls::pki_types::CertificateDer::from(
        authority.cert.der().to_vec(),
    )];
    let key = rustls::pki_types::PrivateKeyDer::try_from(authority.signing_key.serialize_der()).unwrap();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    let body = "the backend spoke tls\n";
                    let _ = stream
                        .write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                                .as_bytes(),
                        )
                        .await;
                }
            });
        }
    });
    (address, authority.cert.pem())
}

#[tokio::test]
async fn a_backend_that_speaks_tls_is_reached_over_tls() {
    let gateway = Gateway::new("upstream-tls");
    let port = free_port();
    let (backend, authority) = tls_backend("api.internal").await;
    std::fs::write(gateway.directory.join("upstream-ca.pem"), &authority).unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{backend}"
    tls server-name="api.internal" ca="file:upstream-ca.pem"
}}
site "*" id="all" {{ route "/*" id="any" {{ proxy upstream="api" }} }}
"#
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(
        response.contains("the backend spoke tls"),
        "the connection has to be TLS, not plaintext: {response}"
    );
}

#[tokio::test]
async fn an_upstream_authority_that_will_not_load_stops_the_reload() {
    let gateway = Gateway::new("upstream-tls-bad-ca");
    let port = free_port();
    let good = format!(
        r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/*" id="any" {{ respond "serving\n" }} }}
"#
    );
    gateway.apply(&good).await;

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "127.0.0.1:9"
    tls server-name="api.internal" ca="file:absent.pem"
}}
site "*" id="all" {{ route "/*" id="any" {{ proxy upstream="api" }} }}
"#
        ))
        .await;
    // Not a connection error under traffic, where it would look like the
    // backend's fault.
    let gatel_plan::Outcome::PrepareFailed(detail) = outcome else {
        panic!("expected the reload to be refused: {outcome:?}");
    };
    assert!(detail.contains("upstream `api`"), "{detail}");
    assert!(detail.contains("cannot read the upstream CA"), "{detail}");

    // The previous configuration is untouched.
    assert!(get(gateway.address(), "/").await.contains("serving"));
}

#[tokio::test]
async fn a_backend_certificate_from_nobody_it_trusts_is_refused() {
    let gateway = Gateway::new("upstream-tls-untrusted");
    let port = free_port();
    let (backend, _authority) = tls_backend("api.internal").await;
    // A different authority: the one that signed the backend is not named.
    let other = rcgen::generate_simple_self_signed(vec!["other".to_string()]).unwrap();
    std::fs::write(gateway.directory.join("upstream-ca.pem"), other.cert.pem()).unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{backend}"
    tls server-name="api.internal" ca="file:upstream-ca.pem"
}}
site "*" id="all" {{ route "/*" id="any" {{ proxy upstream="api" }} }}
"#
        ))
        .await;

    // There is no way to turn verification off, so this fails — which is the
    // point. A gateway that would accept any certificate is a gateway whose
    // TLS to the backend means nothing.
    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 50"), "{response}");
    assert!(!response.contains("the backend spoke tls"), "{response}");
}
