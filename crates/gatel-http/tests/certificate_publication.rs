//! A late preparation failure must not change certificates served by old listeners.
mod support;
use std::sync::Arc;

use gatel_model::{ResourceId, ResourceKind};
use gatel_plan::Outcome;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn changing_source_or_host_replaces_the_certificate_but_compatible_reload_keeps_it() {
    let gateway = Gateway::new("certificate-source");
    let port = free_port();
    let manual = manual_certificate(&gateway, "secure.example.com");
    let config = |host: &str, tls: &str, body: &str| {
        format!(
            r#"
global {{ https "127.0.0.1:{port}"; }}
site "{host}" id="secure" {{ {tls}
route "/" {{ respond "{body}"; }}
}}
"#
        )
    };
    assert!(matches!(
        gateway
            .apply(&config("secure.example.com", &manual, "manual"))
            .await,
        Outcome::Started { .. }
    ));
    let site = ResourceId::new(ResourceKind::Site, "secure");
    let original = gateway.certificates.get(&site).unwrap();
    assert!(matches!(
        gateway
            .apply(&config("secure.example.com", "tls internal", "internal"))
            .await,
        Outcome::Reloaded { .. }
    ));
    let internal = gateway.certificates.get(&site).unwrap();
    assert_ne!(original.certified.cert, internal.certified.cert);
    assert!(matches!(
        gateway
            .apply(&config("secure.example.com", "tls internal", "compatible"))
            .await,
        Outcome::Reloaded { .. }
    ));
    assert!(Arc::ptr_eq(
        &internal.certified,
        &gateway.certificates.get(&site).unwrap().certified
    ));
    assert!(matches!(
        gateway
            .apply(&config("other.example.com", "tls internal", "renamed"))
            .await,
        Outcome::Reloaded { .. }
    ));
    assert_ne!(
        internal.certified.cert,
        gateway.certificates.get(&site).unwrap().certified.cert
    );
    let retired = Arc::downgrade(&internal.certified);
    drop(internal);
    assert_eq!(
        retired.strong_count(),
        0,
        "published runtime must not retain the retired key"
    );
    assert!(matches!(
        gateway
            .apply(&config("secure.example.com", &manual, "restored"))
            .await,
        Outcome::Reloaded { .. }
    ));
    https(&gateway, &original, "restored").await;
    // Removing TLS revokes the slot at commit, not later service reconciliation.
    let plain = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{plain}\"; }}\nsite \"*\" id=\"secure\" {{ route \"/\" {{ respond \"plain\"; }} }}")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    assert!(gateway.certificates.is_empty());
    gateway.stop().await;
}

fn prepare_candidate(
    gateway: &Gateway,
    preparation: gatel_plan::Preparation<'_, Arc<gatel_http::Runtime>>,
) -> Result<Arc<gatel_http::Runtime>, String> {
    gatel_http::prepare(
        &preparation,
        &gatel_http::Resources {
            blocking_budget: None,
            blocking: gateway.server.blocking_executor(),
            plugin_retirements: gateway.server.plugin_retirements(),
            binds: &gateway.binds,
            state: &gateway.state,
            certificates: &gateway.certificates,
            challenges: &gateway.challenges,
            acme: &|_| Err("test does not use ACME".into()),
            telemetry: &gateway.telemetry,
            root: &gateway.directory,
            internal_ca: gateway.directory.join("ca"),
        },
    )
}

#[tokio::test]
async fn preparation_overtaken_by_a_commit_cannot_replace_the_winners_certificate() {
    let gateway = Gateway::new("certificate-stale");
    let port = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");
    let initial = format!(
        r#"
global {{ https "127.0.0.1:{port}"; }}
site "secure.example.com" id="secure" {{ {tls}
route "/" {{ respond "original"; }}
}}
"#
    );
    assert!(matches!(gateway.apply(&initial).await, Outcome::Started { .. }));
    let site = ResourceId::new(ResourceKind::Site, "secure");
    let path = gateway.directory.join("gatel.kdl");
    std::fs::write(&path, initial.replace("original", "winner")).unwrap();
    let mut source = gatel_kdl::snapshot(&path, gatel_model::SourceId::file("main"), None).unwrap();
    source.rev = gatel_model::SourceRev(2);
    let snapshot = gatel_model::Snapshot::new().with(source);
    manual_certificate(&gateway, "secure.example.com");
    let mut winner_certificate = None;
    let outcome = gateway
        .store
        .attempt(&snapshot, &gateway.registry, |preparation| {
            let outcome = gateway
                .store
                .attempt(&snapshot, &gateway.registry, |preparation| {
                    prepare_candidate(&gateway, preparation)
                });
            assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
            winner_certificate = gateway.certificates.get(&site);
            // This candidate resumes only after the winner committed, and reads a
            // different valid file. Its final stale check must discard this write.
            manual_certificate(&gateway, "secure.example.com");
            prepare_candidate(&gateway, preparation)
        });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    let winner = winner_certificate.unwrap();
    assert_eq!(
        gateway.certificates.get(&site).unwrap().certified.cert,
        winner.certified.cert
    );
    https(&gateway, &winner, "winner").await;
    gateway.stop().await;
}

async fn https(gateway: &Gateway, certificate: &gatel_tls::Certificate, body: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.certified.cert[0].clone()).unwrap();
        let client = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let tcp = tokio::net::TcpStream::connect(gateway.address()).await.unwrap();
        let mut stream = connector
            .connect("secure.example.com".try_into().unwrap(), tcp)
            .await
            .unwrap();
        assert_eq!(
            stream.get_ref().1.peer_certificates().unwrap()[0],
            certificate.certified.cert[0]
        );
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: secure.example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).ends_with(body));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_client_ca_loading_cannot_publish_an_already_loaded_server_certificate() {
    let gateway = Gateway::new("certificate-publication");
    let port = free_port();
    let tls = manual_certificate(&gateway, "secure.example.com");
    let config = |fail: bool, body: &str| {
        let auth = if fail {
            "{ client-auth ca=\"absent.pem\"; }"
        } else {
            ""
        };
        format!(
            r#"
global {{ https "127.0.0.1:{port}"; }}
site "secure.example.com" id="secure" {{
{tls} {auth}
route "/" {{ respond "{body}"; }}
}}
"#
        )
    };
    let outcome = gateway.apply(&config(false, "original")).await;
    assert!(matches!(outcome, Outcome::Started { .. }), "{outcome:?}");
    let site = ResourceId::new(ResourceKind::Site, "secure");
    let original = gateway.certificates.get(&site).unwrap();
    https(&gateway, &original, "original").await;
    manual_certificate(&gateway, "secure.example.com");
    let outcome = gateway.apply(&config(true, "invalid")).await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert_eq!(
        gateway.certificates.get(&site).unwrap().certified.cert,
        original.certified.cert
    );
    https(&gateway, &original, "original").await;
    let outcome = gateway.apply(&config(false, "updated")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    let updated = gateway.certificates.get(&site).unwrap();
    assert_ne!(updated.certified.cert, original.certified.cert);
    https(&gateway, &updated, "updated").await;
    gateway.stop().await;
}
