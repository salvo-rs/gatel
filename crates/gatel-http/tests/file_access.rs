//! Read-only terminal semantics and Windows alternate-stream isolation.
mod support;
use support::*;

#[tokio::test]
async fn static_terminal_rejects_other_methods_without_invalidating_cached_gets() {
    let gateway = Gateway::new("file-read-only");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "static bytes").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ cache default-ttl=\"60s\"; file-server root=\"{}\" browse=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    assert!(
        get(gateway.address(), "/page.txt")
            .await
            .ends_with("static bytes")
    );
    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE"] {
        for path in ["/page.txt", "/", "/absent"] {
            let response = request(gateway.address(), &format!("{method} {path} HTTP/1.1\r\nHost: example.com\r\nIf-None-Match: *\r\nConnection: close\r\n\r\n")).await;
            assert!(
                response.starts_with("HTTP/1.1 405"),
                "{method} {path}: {response}"
            );
            assert!(
                response.to_lowercase().contains("allow: get, head") && response.contains("no-store"),
                "{response}"
            );
            assert!(
                !response.ends_with("static bytes") && !response.contains("<ul>"),
                "{response}"
            );
        }
    }
    let cached = get(gateway.address(), "/page.txt").await;
    assert!(
        cached.contains("hit;") && cached.ends_with("static bytes"),
        "{cached}"
    );
    let head = request(
        gateway.address(),
        "HEAD /page.txt HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        head.starts_with("HTTP/1.1 200") && head.ends_with("\r\n\r\n"),
        "{head}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_alternate_streams_are_not_http_files() {
    let gateway = Gateway::new("file-ntfs-stream");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "public content").unwrap();
    let stream = root.join("page.txt:secret");
    std::fs::write(&stream, "private stream bytes").unwrap();
    assert_eq!(std::fs::read_to_string(&stream).unwrap(), "private stream bytes");
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" browse=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for path in [
        "/page.txt:secret",
        "/page.txt%3Asecret",
        "/page.txt%3A%3A%24DATA",
        "/NUL",
        "/CON.txt",
        "/page.txt.",
        "/page.txt%20",
    ] {
        let response = get(gateway.address(), path).await;
        assert!(
            response.starts_with("HTTP/1.1 404") && !response.contains("private stream bytes"),
            "{path}: {response}"
        );
    }
    assert!(
        get(gateway.address(), "/page.txt")
            .await
            .ends_with("public content")
    );
}
