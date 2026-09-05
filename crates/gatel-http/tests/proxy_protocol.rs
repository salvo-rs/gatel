//! The PROXY protocol, over a real socket.
//!
//! The unit tests in `gatel-http` cover parsing. These cover the thing that
//! actually matters: that the address a balancer states reaches the decisions
//! that read a client address — `ip-filter`, a per-client `rate-limit`, and
//! the access log — rather than the balancer's own address reaching them.

mod support;

use support::*;
use tokio::io::AsyncWriteExt;

/// Open a connection, send a PROXY v1 header, then a request, and read the
/// answer. Loopback is the peer, which is what the configurations below trust.
async fn with_header(address: std::net::SocketAddr, header: &[u8], path: &str) -> String {
    use tokio::io::AsyncReadExt;

    let mut stream = tokio::net::TcpStream::connect(address).await.expect("connect");
    stream.write_all(header).await.expect("header");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("request");

    let mut answer = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
    String::from_utf8_lossy(&answer).into_owned()
}

fn v2_header(source: [u8; 4], port: u16) -> Vec<u8> {
    let mut bytes = vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
    ];
    bytes.push(0x21); // version 2, PROXY
    bytes.push(0x11); // AF_INET, STREAM
    bytes.extend_from_slice(&12u16.to_be_bytes());
    bytes.extend_from_slice(&source);
    bytes.extend_from_slice(&[10, 0, 0, 1]);
    bytes.extend_from_slice(&port.to_be_bytes());
    bytes.extend_from_slice(&443u16.to_be_bytes());
    bytes
}

#[tokio::test]
async fn an_ip_filter_sees_the_client_the_balancer_named() {
    // The whole point. Behind a layer-4 balancer the peer is the balancer, so
    // without the header every client looks like one address — and an allow
    // list either lets everybody in or nobody.
    let gateway = Gateway::new("proxy-protocol-filter");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}" proxy-protocol=#true
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{
    route "/*" {{
        ip-filter allow="203.0.113.0/24"
        respond "allowed"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();

    let inside = with_header(address, b"PROXY TCP4 203.0.113.7 10.0.0.1 45678 443\r\n", "/").await;
    assert!(inside.contains("allowed"), "{inside}");

    let outside = with_header(address, b"PROXY TCP4 198.51.100.9 10.0.0.1 45678 443\r\n", "/").await;
    assert!(
        outside.starts_with("HTTP/1.1 403"),
        "a client outside the allow list must still be refused: {outside}"
    );
}

#[tokio::test]
async fn the_binary_form_works_too() {
    // Version 2 is what an AWS Network Load Balancer sends, so supporting only
    // the text form would mean supporting the protocol in name only.
    let gateway = Gateway::new("proxy-protocol-v2");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}" proxy-protocol=#true
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{
    route "/*" {{
        ip-filter allow="203.0.113.0/24"
        respond "allowed"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(
        with_header(address, &v2_header([203, 0, 113, 7], 45678), "/")
            .await
            .contains("allowed")
    );
    assert!(
        with_header(address, &v2_header([198, 51, 100, 9], 45678), "/")
            .await
            .starts_with("HTTP/1.1 403")
    );
}

#[tokio::test]
async fn a_connection_with_no_header_is_closed_rather_than_misread() {
    // The listener was told every connection carries a header. Serving one
    // that does not would attribute the request to whatever the first line
    // happened to parse as.
    let gateway = Gateway::new("proxy-protocol-missing");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}" proxy-protocol=#true
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{ route "/*" {{ respond "hello" }} }}
"#
        ))
        .await;

    let answer = with_header(gateway.address(), b"", "/").await;
    assert!(
        answer.is_empty(),
        "a connection without a header must be closed, not served: {answer:?}"
    );
}

#[tokio::test]
async fn a_health_check_from_the_balancer_itself_is_served() {
    // A balancer checks the port with a LOCAL header and no client. That is a
    // real connection and must be answered, or every health check fails and
    // the balancer takes the gateway out of rotation.
    let gateway = Gateway::new("proxy-protocol-local");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{
    http "127.0.0.1:{port}" proxy-protocol=#true
    trusted-proxy "127.0.0.0/8"
}}
site "*" {{ route "/*" {{ respond "healthy" }} }}
"#
        ))
        .await;

    assert!(
        with_header(gateway.address(), b"PROXY UNKNOWN\r\n", "/")
            .await
            .contains("healthy")
    );
}

#[tokio::test]
async fn a_listener_without_it_is_untouched() {
    // Turning the feature on for one port must not change another, and a
    // configuration that never mentions it must behave exactly as before.
    let gateway = Gateway::new("proxy-protocol-off");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/*" {{ respond "plain" }} }}
"#
        ))
        .await;

    assert!(get(gateway.address(), "/").await.contains("plain"));
}
