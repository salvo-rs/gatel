//! Backends reached over a Unix domain socket.
//!
//! How PHP-FPM, gunicorn, uWSGI and a great many sidecars are reached, and the
//! shape a traditional deployment behind nginx has had for twenty years.
//!
//! The whole file is behind `cfg(unix)`. On a platform with no Unix sockets
//! the configuration is refused at compile time rather than failing at the
//! first connection, and `a_unix_backend_is_refused_where_there_are_none` in
//! `gatel-plan` covers that side.

#![cfg(unix)]

mod support;

use std::path::PathBuf;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A backend listening on a socket path rather than a port.
///
/// The socket is removed first: a path left behind by a previous run is not a
/// listener, and binding over it is what every daemon that uses one has to do.
async fn unix_backend(path: PathBuf, body: &'static str) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).expect("bind the socket");
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
                    let answer = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                    if stream.write_all(answer.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    })
}

fn socket_path(gateway: &Gateway, name: &str) -> PathBuf {
    gateway.directory.join(name)
}

#[tokio::test]
async fn a_request_is_proxied_over_a_socket_path() {
    let gateway = Gateway::new("uds-proxy");
    let port = free_port();
    let socket = socket_path(&gateway, "app.sock");
    let _backend = unix_backend(socket.clone(), "from the socket").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{ proxy "unix:{}" }}
}}
"#,
            socket.display()
        ))
        .await;

    let response = get(gateway.address(), "/anything").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("from the socket"), "{response}");
}

#[tokio::test]
async fn connections_to_a_socket_are_pooled_like_any_other() {
    // A Unix connection has no address, and the pool tracks which address a
    // socket reached. If that had been left as a required field, either the
    // connection would not be parked or it would be parked under a wrong one.
    let gateway = Gateway::new("uds-pool");
    let port = free_port();
    let socket = socket_path(&gateway, "pool.sock");
    let _backend = unix_backend(socket.clone(), "pooled").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "app" {{
    endpoint "unix:{}"
    pool max-per-endpoint=4
}}
site "*" {{ route "/*" {{ proxy upstream="app" }} }}
"#,
            socket.display()
        ))
        .await;

    let address = gateway.address();
    for _ in 0..5 {
        assert!(get(address, "/").await.contains("pooled"));
    }

    let stats = gateway
        .store
        .current()
        .expect("a generation")
        .runtime
        .upstreams
        .iter()
        .flat_map(|upstream| upstream.pool.stats())
        .collect::<Vec<_>>();
    let app = stats.first().expect("the upstream has a pool");
    assert!(
        app.reused > 0,
        "connections to a socket must be reused like any other: {app:?}"
    );
}

#[tokio::test]
async fn a_socket_that_is_not_there_is_a_backend_that_is_down() {
    // Not a configuration error: the path may be created a moment later by the
    // application that owns it, exactly as an address may start listening.
    let gateway = Gateway::new("uds-missing");
    let port = free_port();
    let socket = socket_path(&gateway, "absent.sock");

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "app" {{
    endpoint "unix:{}"
    timeouts queue="1s" connect="1s" response-head="1s" overall="5s"
}}
site "*" {{ route "/*" {{ proxy upstream="app" }} }}
"#,
            socket.display()
        ))
        .await;

    let response = get(gateway.address(), "/").await;
    assert!(
        response.starts_with("HTTP/1.1 50"),
        "an unreachable socket is a gateway error: {response}"
    );
}

#[tokio::test]
async fn a_socket_and_an_address_can_be_backends_of_one_upstream() {
    // A migration from one to the other should not need two upstreams.
    let gateway = Gateway::new("uds-mixed");
    let port = free_port();
    let socket = socket_path(&gateway, "mixed.sock");
    let _unix = unix_backend(socket.clone(), "answered").await;
    let tcp = backend("answered").await;

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "app" {{
    endpoint "unix:{}" id="socket"
    endpoint "{tcp}" id="address"
    balance "round-robin"
}}
site "*" {{ route "/*" {{ proxy upstream="app" }} }}
"#,
            socket.display()
        ))
        .await;

    let address = gateway.address();
    for _ in 0..4 {
        let response = get(address, "/").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("answered"), "{response}");
    }
}
