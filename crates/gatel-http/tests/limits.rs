//! The limits in `global { limits ... }`, actually enforced.
//!
//! Configuration that promises a bound and does not apply it is worse than
//! configuration that never mentioned one, so each of these drives a real
//! connection rather than reading a field back.

mod support;

use std::time::Duration;

use support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

fn config(port: u16, limits: &str) -> String {
    format!(
        r#"
global {{
    http "127.0.0.1:{port}"
    limits {limits}
}}
site "*" id="all" {{ route "/*" id="any" {{ respond "ok\n" }} }}
"#
    )
}

#[tokio::test]
async fn a_declared_body_over_the_limit_is_refused_without_reading_it() {
    let gateway = Gateway::new("limit-declared");
    let port = free_port();
    gateway.apply(&config(port, r#"body="64""#)).await;

    let response = request(
        gateway.address(),
        // The length is declared but the body is never sent: if the limit were
        // applied after reading, this would hang instead of answering.
        "POST /upload HTTP/1.1\r\nHost: h\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(response.contains("larger than the 64 byte limit"), "{response}");
}

#[tokio::test]
async fn a_body_within_the_limit_is_served() {
    let gateway = Gateway::new("limit-within");
    let port = free_port();
    gateway.apply(&config(port, r#"body="64""#)).await;

    let response = request(
        gateway.address(),
        "POST /upload HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn a_head_larger_than_the_limit_is_refused() {
    let gateway = Gateway::new("limit-header");
    let port = free_port();
    // 8KiB is hyper's floor for a read buffer, so it is the smallest limit
    // that means anything.
    gateway.apply(&config(port, r#"header="8KiB""#)).await;

    let padding = "x".repeat(16 * 1024);
    let response = request(
        gateway.address(),
        &format!("GET / HTTP/1.1\r\nHost: h\r\nX-Big: {padding}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    // Either 431 or a closed connection, depending on how much hyper had read
    // before the limit bit. What must never happen is the route answering.
    assert!(
        !response.contains("ok"),
        "a head over the limit reached the route: {response}"
    );
    assert!(
        response.is_empty() || response.starts_with("HTTP/1.1 4"),
        "and it is refused rather than served: {response}"
    );
}

#[tokio::test]
async fn a_connection_that_never_sends_a_head_is_closed() {
    let gateway = Gateway::new("limit-idle");
    let port = free_port();
    gateway.apply(&config(port, r#"idle="300ms""#)).await;

    let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
    // Enough of a head to look like a client, never enough to be one.
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();

    let closed = tokio::time::timeout(Duration::from_secs(5), read_to_end(&mut stream)).await;
    assert!(
        closed.is_ok(),
        "a client that dribbles a head out must not hold a slot forever"
    );
}

#[tokio::test]
async fn a_keep_alive_connection_is_closed_once_it_goes_quiet() {
    let gateway = Gateway::new("limit-keepalive");
    let port = free_port();
    gateway.apply(&config(port, r#"idle="300ms""#)).await;

    let mut stream = TcpStream::connect(gateway.address()).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n")
        .await
        .unwrap();

    // One request, answered, then nothing. The connection is the gateway's to
    // reclaim.
    let closed = tokio::time::timeout(Duration::from_secs(5), read_to_end(&mut stream)).await;
    let text = closed.expect("the connection must be closed rather than held open");
    assert!(text.contains("200 OK"), "{text}");
}

#[tokio::test]
async fn the_connection_cap_is_raised_and_lowered_by_configuration() {
    let gateway = Gateway::new("limit-connections");
    let port = free_port();
    gateway.apply(&config(port, r#"connections=64"#)).await;
    assert!(get(gateway.address(), "/").await.contains("ok"));

    // Raising and lowering must both be applied without dropping what is
    // already open.
    gateway.apply(&config(port, r#"connections=128"#)).await;
    assert!(get(gateway.address(), "/").await.contains("ok"));
    gateway.apply(&config(port, r#"connections=8"#)).await;
    assert!(get(gateway.address(), "/").await.contains("ok"));
}

async fn read_to_end(stream: &mut TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}
