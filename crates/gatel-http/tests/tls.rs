//! Terminating TLS.

mod support;

use std::net::SocketAddr;

use gatel_plan::Outcome;
use support::*;

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

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
