//! Listed links must round-trip through the actual request path decoder.
mod support;
use support::*;

#[cfg(unix)]
#[tokio::test]
async fn public_symlink_names_do_not_expose_hidden_targets() {
    use std::os::unix::fs::symlink;
    let gateway = Gateway::new("listing-hidden-targets");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(root.join(".private")).unwrap();
    std::fs::write(root.join(".private/secret.txt"), "private sentinel").unwrap();
    std::fs::write(root.join("public.txt"), "public bytes").unwrap();
    // The hidden sidecar target must be fresh enough that mtime filtering
    // cannot accidentally make the access-policy regression pass.
    std::fs::File::options()
        .write(true)
        .open(root.join(".private/secret.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .unwrap();
    symlink(root.join(".private/secret.txt"), root.join("alias.txt")).unwrap();
    symlink(root.join(".private"), root.join("alias-dir")).unwrap();
    symlink(root.join(".private/secret.txt"), root.join("index.html")).unwrap();
    symlink(root.join(".private/secret.txt"), root.join("public.txt.gz")).unwrap();
    symlink(root.join("public.txt"), root.join("safe-link.txt")).unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" browse=#true precompressed=#true; }} }}", root.display())).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for path in [
        "/alias.txt",
        "/alias-dir/",
        "/alias-dir/secret.txt",
        "/index.html",
    ] {
        let response = get(gateway.address(), path).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
        assert!(!response.contains("private sentinel"), "{response}");
    }
    let listing = get(gateway.address(), "/").await;
    assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
    assert!(listing.contains("./safe-link.txt"), "{listing}");
    for hidden in [
        "alias.txt",
        "alias-dir",
        "index.html",
        "public.txt.gz",
        "private sentinel",
    ] {
        assert!(!listing.contains(hidden), "{hidden}: {listing}");
    }
    let encoded = request(
        gateway.address(),
        "GET /public.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(encoded.ends_with("public bytes"), "{encoded}");
    assert!(!encoded.to_lowercase().contains("content-encoding:"), "{encoded}");
    assert!(
        get(gateway.address(), "/safe-link.txt")
            .await
            .ends_with("public bytes")
    );
}

#[tokio::test]
async fn directory_links_encode_reserved_bytes_and_round_trip_unicode() {
    let gateway = Gateway::new("listing-links");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(root.join("子目录")).unwrap();
    std::fs::write(root.join("a #&%.txt"), "reserved bytes").unwrap();
    std::fs::write(root.join("中文.txt"), "unicode filename").unwrap();
    std::fs::write(root.join(".hidden"), "hidden secret").unwrap();
    std::fs::write(root.join("子目录").join("inside.txt"), "inside").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" browse=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let listing = get(gateway.address(), "/").await;
    assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
    assert!(
        listing.contains("a #&amp;%.txt") && !listing.contains(".hidden"),
        "{listing}"
    );
    for (link, body) in [
        ("a%20%23%26%25.txt", "reserved bytes"),
        ("%E4%B8%AD%E6%96%87.txt", "unicode filename"),
    ] {
        assert!(listing.contains(&format!("href=\"./{link}\"")), "{listing}");
        let response = get(gateway.address(), &format!("/{link}")).await;
        assert!(
            response.starts_with("HTTP/1.1 200") && response.ends_with(body),
            "{response}"
        );
    }
    let directory = "%E5%AD%90%E7%9B%AE%E5%BD%95/";
    assert!(listing.contains(&format!("href=\"./{directory}\"")), "{listing}");
    let nested = get(gateway.address(), &format!("/{directory}")).await;
    assert!(
        nested.contains("href=\"../\"") && nested.contains("./inside.txt"),
        "{nested}"
    );
}
