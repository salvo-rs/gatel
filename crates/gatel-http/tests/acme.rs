//! The parts of automatic certificates that are Gatel's rather than the ACME
//! library's: where a challenge is answered, and what cannot get in its way.

mod support;

use support::*;

const CHALLENGE: &str = "/.well-known/acme-challenge/";

#[tokio::test]
async fn lost_registration_response_reuses_the_same_wire_identity_after_restart() {
    use std::sync::Arc;
    use std::time::Duration;
    let gateway = Gateway::new("acme-registration-loss");
    let authority = support::acme_ca::Authority::start("127.0.0.1:1".parse().unwrap()).await;
    authority.lose_registration_responses();
    let config = gatel_plan::Acme {
        directory: authority.directory_url.clone(),
        email: "ops@example.com".into(),
        agreed: true,
        directory_path: None,
        renew_before: Duration::from_secs(86400),
    };
    for _ in 0..2 {
        let before = authority.registration_keys().len();
        let issuer = Arc::new(
            gatel_tls::acme::Issuer::new(&config, &gateway.directory, gatel_tls::acme::Challenges::new())
                .unwrap(),
        );
        let issuing = issuer.clone();
        let mut attempt = tokio::spawn(async move { issuing.issue("acme.example.com").await });
        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            while authority.registration_keys().len() == before {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    result = &mut attempt => {
                        assert!(authority.registration_keys().len() > before,
                            "issuance ended before the authority received registration: {result:?}");
                        // The completed handle must not be polled a second time.
                        return true;
                    }
                }
            }
            false
        })
        .await
        .expect("authority receives registration before deadline");
        if !completed {
            attempt.abort();
            let _ = attempt.await;
        }
        issuer.drain_storage().await;
    }
    let keys = authority.registration_keys();
    assert!(keys.len() >= 2);
    assert_eq!(keys[0]["kty"], "EC");
    assert!(
        keys.iter().all(|key| key == &keys[0]),
        "restart must reuse the signing identity"
    );
}

async fn wait_for_certificate(gateway: &Gateway) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if gateway.certificates.len() == 1
                && gateway.challenges.is_empty()
                && !gateway
                    .telemetry
                    .health
                    .report()
                    .reasons
                    .iter()
                    .any(|reason| reason.code() == "certificate_pending")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background issuance completes");
}

#[tokio::test]
async fn a_challenge_is_answered_by_the_listener_that_is_already_there() {
    let gateway = Gateway::new("acme-challenge");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/" id="root" {{ respond "site\n" }} }}
"#
        ))
        .await;

    gateway.challenges.present("tokenAAA", "tokenAAA.thumbprint");

    // No route mentions this path, and none has to: the challenge is not a
    // route, it is the listener answering the certificate authority.
    let response = get(gateway.address(), &format!("{CHALLENGE}tokenAAA")).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("tokenAAA.thumbprint"), "{response}");
}

#[tokio::test]
async fn nothing_a_site_declares_can_get_in_the_way_of_one() {
    let gateway = Gateway::new("acme-guards");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    force-https
    ip-filter allow="10.0.0.0/8"
    rate-limit max=1 window="1h"
    route "/" id="root" {{ respond "site\n" }}
}}
"#
        ))
        .await;
    gateway.challenges.present("tokenAAA", "answer-for-the-ca");

    // Every one of those would refuse or redirect an ordinary request from
    // this test, and a certificate authority is not an ordinary request: a
    // redirect to a URL it cannot reach, or a 403, is a certificate that never
    // arrives.
    let refused = get(gateway.address(), "/").await;
    assert!(refused.starts_with("HTTP/1.1 308"), "{refused}");

    let answered = get(gateway.address(), &format!("{CHALLENGE}tokenAAA")).await;
    assert!(answered.starts_with("HTTP/1.1 200"), "{answered}");
    assert!(answered.ends_with("answer-for-the-ca"), "{answered}");
}

#[tokio::test]
async fn a_challenge_survives_a_reload() {
    let gateway = Gateway::new("acme-reload");
    let port = free_port();
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/" id="root" {{ respond "{body}" }} }}
"#
        )
    };
    gateway.apply(&config("first")).await;
    gateway.challenges.present("tokenAAA", "answer");

    // An order takes seconds to minutes. An edit made while one is in flight
    // must not lose the certificate it would have produced.
    gateway.apply(&config("second")).await;

    let answered = get(gateway.address(), &format!("{CHALLENGE}tokenAAA")).await;
    assert!(answered.ends_with("answer"), "{answered}");
}

#[tokio::test]
async fn a_token_nobody_is_waiting_for_is_not_answered() {
    let gateway = Gateway::new("acme-unknown");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/" id="root" {{ respond "site\n" }} }}
"#
        ))
        .await;

    // Falls through to routing, which has nothing for it.
    let response = get(gateway.address(), &format!("{CHALLENGE}nobodyAsked")).await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}

#[tokio::test]
async fn a_path_shaped_like_a_challenge_cannot_read_anything_else() {
    let gateway = Gateway::new("acme-traversal");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{ route "/*" id="any" {{ respond "site\n" }} }}
"#
        ))
        .await;
    gateway.challenges.present("tokenAAA", "answer");

    for path in ["../../etc/passwd", "tokenAAA/../tokenAAA", "token%20AAA"] {
        let response = get(gateway.address(), &format!("{CHALLENGE}{path}")).await;
        assert!(
            !response.contains("answer"),
            "`{path}` reached the challenge store: {response}"
        );
    }
}

// ---------------------------------------------------------------------------
// A whole order, end to end
// ---------------------------------------------------------------------------

/// From nothing to a certificate this process can present.
///
/// The authority here is a test one, but the client is the real ACME client and
/// it really fetches the challenge from the gateway before it will sign
/// anything — so what passes is Gatel's side of the exchange.
#[tokio::test]
async fn a_certificate_is_obtained_and_then_served() {
    let gateway = Gateway::new("acme-order");
    let http = free_port();
    let https = free_port();

    // The authority needs to know where to fetch the challenge from, because
    // there is no DNS in a test.
    let authority = support::acme_ca::Authority::start(format!("127.0.0.1:{http}").parse().unwrap()).await;

    let outcome = gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{http}"
    https "127.0.0.1:{https}"
    acme email="ops@example.com" agreed=#true ca="{}" directory="acme-state"
}}
site "acme.example.com" id="secure" {{
    tls acme
    route "/" id="root" {{ respond "secure\n" }}
}}
"#,
            authority.directory_url
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "a listener with no certificate yet still starts: {outcome:?}"
    );
    assert!(
        gateway.certificates.is_empty(),
        "and has nothing to present until the order finishes"
    );

    // Activation starts an owned background sweep without waiting for the CA.
    gateway.reconcile_acme().await;
    wait_for_certificate(&gateway).await;

    let fetched = authority.fetched();
    assert_eq!(fetched.len(), 1, "the authority asked exactly once: {fetched:?}");
    let (token, answer) = &fetched[0];
    assert!(
        answer.starts_with(token),
        "the gateway answered with the key authorization: {answer}"
    );

    assert_eq!(
        gateway.certificates.len(),
        1,
        "the issued certificate is in the store, ready for a handshake"
    );
    assert!(
        gateway.challenges.is_empty(),
        "and the challenge is no longer answerable"
    );

    // Readiness stops complaining once the certificate is there.
    let report = gateway.telemetry.health.report();
    assert!(
        !report
            .reasons
            .iter()
            .any(|reason| reason.code() == "certificate_pending"),
        "{report:?}"
    );
}

#[tokio::test]
async fn a_certificate_is_kept_across_a_restart() {
    let gateway = Gateway::new("acme-restart");
    let http = free_port();
    let https = free_port();
    let authority = support::acme_ca::Authority::start(format!("127.0.0.1:{http}").parse().unwrap()).await;
    let config = |http: u16, https: u16| {
        format!(
            r#"
global {{
    http "127.0.0.1:{http}"
    https "127.0.0.1:{https}"
    acme email="ops@example.com" agreed=#true ca="{}" directory="acme-state"
}}
site "acme.example.com" id="secure" {{
    tls acme
    route "/" id="root" {{ respond "secure\n" }}
}}
"#,
            authority.directory_url
        )
    };
    gateway.apply(&config(http, https)).await;
    gateway.reconcile_acme().await;
    wait_for_certificate(&gateway).await;
    assert_eq!(gateway.certificates.len(), 1);

    // A second process, same directory. It must serve what the first obtained
    // rather than ordering again — a restart loop that reorders is how an
    // account reaches a rate limit.
    let committed = gateway.store.current().unwrap();
    let state = gatel_tls::Issuer::storage_directory(
        committed.plan.spec.global.acme.as_ref().unwrap(),
        &gateway.directory,
    );
    let listing: Vec<String> = std::fs::read_dir(&state)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        listing.iter().any(|name| name.ends_with(".identity")),
        "the certificate was written where a restart will look: {listing:?}"
    );

    // A process exiting releases its sockets; the directory stays.
    gateway.stop().await;

    let restarted = Gateway::reopen(&gateway);
    // Fresh ports, because both gateways are one process and only one of them
    // can have a port. What a restart shares is the directory, and that is what
    // this is about.
    let outcome = restarted.apply(&config(free_port(), free_port())).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        restarted.certificates.len(),
        1,
        "the certificate on disk was adopted during prepare"
    );
    assert_eq!(
        authority.fetched().len(),
        1,
        "and nothing was ordered a second time"
    );
}

#[tokio::test]
async fn an_authority_that_refuses_does_not_stop_the_gateway_serving() {
    let gateway = Gateway::new("acme-refused");
    let http = free_port();
    let https = free_port();
    // Nothing is listening there.
    gateway
        .apply(&format!(
            r#"
global {{
    http "http" "127.0.0.1:{http}"
    https "127.0.0.1:{https}"
    acme email="ops@example.com" agreed=#true ca="http://127.0.0.1:1/directory"
}}
site "acme.example.com" id="secure" {{
    tls acme
    route "/*" id="secured" {{ respond "over tls\n" }}
}}
site "*" id="plain" listener="http" {{
    route "/*" id="root" {{ respond "plaintext still works\n" }}
}}
"#
        ))
        .await;
    gateway.reconcile_acme().await;

    // The plaintext listener is serving, and readiness says what is wrong
    // rather than taking the instance out of rotation over it.
    let response = request(
        format!("127.0.0.1:{http}").parse().unwrap(),
        "GET / HTTP/1.1\r\nHost: acme.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.contains("plaintext still works"), "{response}");

    let report = gateway.telemetry.health.report();
    assert_eq!(report.state, gatel_observe::State::Degraded, "{report:?}");
    assert!(
        report
            .reasons
            .iter()
            .any(|reason| reason.code() == "certificate_pending"),
        "{report:?}"
    );
}
