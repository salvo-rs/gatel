//! Authorization and file lookup must agree about equivalent path spellings.
mod support;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use support::*;

const ALIASES: &[&str] = &[
    "/private/example.txt",
    "/%70rivate/example.txt",
    "//private/example.txt",
    "/./private/example.txt",
    "/public/../private/example.txt",
    "/private%2fexample.txt",
    "/private%5cexample.txt",
];

#[tokio::test]
async fn path_aliases_cannot_select_an_unprotected_file_route_on_h1_or_h2() {
    let gateway = Gateway::new("routing-paths");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(root.join("private")).unwrap();
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::write(root.join("private/example.txt"), "private fixture").unwrap();
    std::fs::write(root.join("public/example.txt"), "public fixture").unwrap();
    let hash = bcrypt::hash("fixture", 4).unwrap();
    write_key(&gateway.directory.join("auth.hash"), &hash);
    let port = free_port();
    let config = format!(
        "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{\nroute \"/private/*\" {{ basic-auth {{ user \"admin\" hash=\"auth.hash\"; }}; file-server root=\"www\"; }}\nroute \"/*\" {{ file-server root=\"www\"; }}\n}}"
    );
    assert!(matches!(
        gateway.apply(&config).await,
        gatel_plan::Outcome::Started { .. }
    ));
    for path in ALIASES {
        let answer = get(gateway.address(), path).await;
        assert!(answer.starts_with("HTTP/1.1 401"), "{path}: {answer}");
        assert!(!answer.contains("private fixture"));
    }
    assert!(
        get(gateway.address(), "/public/example.txt")
            .await
            .ends_with("public fixture")
    );
    // Authorization sees the decoded identity; successful delivery still uses
    // exactly one decode of the original escaped URI.
    let authorized = request(gateway.address(), "GET /%70rivate/example.txt HTTP/1.1\r\nHost: example.com\r\nAuthorization: Basic YWRtaW46Zml4dHVyZQ==\r\nConnection: close\r\n\r\n").await;
    assert!(
        authorized.starts_with("HTTP/1.1 200") && authorized.ends_with("private fixture"),
        "{authorized}"
    );
    #[cfg(windows)]
    for path in [
        "/PRIVATE/example.txt",
        "/Private/EXAMPLE.TXT",
        "/%50rivate/example.txt",
    ] {
        let answer = get(gateway.address(), path).await;
        assert!(answer.starts_with("HTTP/1.1 404"), "{path}: {answer}");
        assert!(!answer.contains("private fixture"));
    }
    let io = TokioIo::new(tokio::net::TcpStream::connect(gateway.address()).await.unwrap());
    let (mut client, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    for path in ALIASES {
        let answer = client
            .send_request(
                http::Request::builder()
                    .uri(format!("http://example.com{path}"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(answer.status(), 401, "{path}");
        let body = answer.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains("private fixture"));
    }
    drop(client);
    driver.abort();
    let _ = driver.await;
    gateway.stop().await;
}

#[tokio::test]
async fn encoded_route_declarations_and_requests_use_the_same_identity() {
    let gateway = Gateway::new("encoded-route");
    let port = free_port();
    let config = format!(
        "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/priv%61te/*\" {{ respond \"denied\" status=403; }}; route \"/*\" {{ respond \"public\"; }}; }}"
    );
    assert!(matches!(
        gateway.apply(&config).await,
        gatel_plan::Outcome::Started { .. }
    ));
    for path in ["/private/file", "/priv%61te/file", "/private%2ffile"] {
        let answer = get(gateway.address(), path).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{path}: {answer}");
    }
    // Neither query escaping nor double encoding is interpreted as a second path.
    assert!(
        get(gateway.address(), "/%2570rivate/file?next=%2Fprivate")
            .await
            .ends_with("public")
    );
    gateway.stop().await;
}

#[tokio::test]
async fn escaped_literal_asterisks_do_not_collide_with_prefix_route_identities() {
    let gateway = Gateway::new("literal-star-routes");
    let port = free_port();
    let config = format!(
        "global {{ http \"127.0.0.1:{port}\"; }}\nsite \"*\" {{ route \"/literal%2A\" {{ respond \"literal\"; }}; route \"/literal*\" {{ respond \"prefix\"; }}; }}"
    );
    let outcome = gateway.apply(&config).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    assert!(get(gateway.address(), "/literal%2A").await.ends_with("literal"));
    assert!(get(gateway.address(), "/literal-other").await.ends_with("prefix"));
    gateway.stop().await;
}
