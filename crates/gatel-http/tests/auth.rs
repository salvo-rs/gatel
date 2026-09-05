//! Authentication and rewriting, over a real socket.

mod support;

use support::*;

/// A bcrypt hash for a known password, written where the configuration can
/// reference it.
fn write_hash(gateway: &Gateway, name: &str, password: &str) -> String {
    // Cost 4: the lowest bcrypt allows, so a test is not slower than the thing
    // it is testing.
    let hash = bcrypt::hash(password, 4).unwrap();
    let path = gateway.directory.join(format!("{name}.hash"));
    // A file of password hashes is a secret, and loading refuses one that
    // anybody on the machine can read.
    write_key(&path, &hash);
    format!("{name}.hash")
}

fn basic(name: &str, password: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{name}:{password}");
    let bytes = raw.as_bytes();
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(TABLE[((value >> (18 - index * 6)) & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[tokio::test]
async fn a_protected_route_challenges_and_then_admits() {
    let gateway = Gateway::new("auth");
    let port = free_port();
    let reference = write_hash(&gateway, "admin", "hunter2");

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/open" {{ respond "public" }}
    route "/private" {{
        respond "secret"
        basic-auth realm="staging" {{
            user "admin" hash="{reference}"
        }}
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(get(address, "/open").await.contains("public"));

    // No credentials: a challenge naming the realm, and no body from the
    // terminal that was written after the guard.
    let challenged = get(address, "/private").await;
    assert!(challenged.starts_with("HTTP/1.1 401"), "{challenged}");
    assert!(
        challenged.to_lowercase().contains("www-authenticate: basic"),
        "{challenged}"
    );
    assert!(challenged.contains("staging"));
    assert!(!challenged.contains("secret"));

    let authorised = request(
        address,
        &format!(
            "GET /private HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic {}\r\nConnection: close\r\n\r\n",
            basic("admin", "hunter2")
        ),
    )
    .await;
    assert!(authorised.contains("secret"), "{authorised}");

    let wrong = request(
        address,
        &format!(
            "GET /private HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic {}\r\nConnection: close\r\n\r\n",
            basic("admin", "wrong")
        ),
    )
    .await;
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
}

#[tokio::test]
async fn a_hash_that_cannot_be_read_stops_the_reload() {
    let gateway = Gateway::new("auth-missing");
    let port = free_port();
    let reference = write_hash(&gateway, "admin", "hunter2");

    let config = |hash: &str| {
        format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ respond \"ok\"\n basic-auth {{ user \"admin\" hash=\"{hash}\" }} }} }}\n"
        )
    };
    gateway.apply(&config(&reference)).await;
    assert_eq!(gateway.store.status().generation.number(), 1);

    let outcome = gateway.apply(&config("absent.hash")).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::PrepareFailed(_)),
        "{outcome:?}"
    );
    assert_eq!(
        gateway.store.status().generation.number(),
        1,
        "the previous generation still serves"
    );
}

#[tokio::test]
async fn a_rewrite_changes_the_path_the_backend_sees() {
    let gateway = Gateway::new("rewrite");
    let port = free_port();
    let upstream = backend("api").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/api/*" {{
        rewrite strip-prefix="/api"
        proxy "{upstream}"
    }}
}}
"#
        ))
        .await;

    // The harness backend echoes the path it saw.
    let response = get(gateway.address(), "/api/users").await;
    assert!(response.contains("api"), "{response}");
}

#[tokio::test]
async fn a_rewrite_cannot_carry_a_request_past_a_guard() {
    let gateway = Gateway::new("rewrite-guard");
    let port = free_port();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        respond "reached"
        rewrite to="/somewhere-else"
        ip-filter deny="0.0.0.0/0"
    }}
}}
"#
        ))
        .await;

    // The guard phase runs before the request phase, so the filter sees the
    // request whatever the rewrite would have done to it.
    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
}
