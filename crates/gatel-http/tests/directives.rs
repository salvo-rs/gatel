//! The standard directives, end to end.

mod support;

use std::net::SocketAddr;

use gatel_plan::Outcome;
use support::*;

// ---------------------------------------------------------------------------
// Guards and directive state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rate_limit_refuses_once_the_allowance_is_spent() {
    let gateway = Gateway::new("rate-limit");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max=2 window="1h"
        respond "allowed"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(get(address, "/").await.contains("allowed"));
    assert!(get(address, "/").await.contains("allowed"));

    let refused = get(address, "/").await;
    assert!(refused.starts_with("HTTP/1.1 429"), "{refused}");
    assert!(
        refused.contains("retry-after:"),
        "a client must be told when to come back: {refused}"
    );
    assert!(refused.contains("x-ratelimit-remaining: 0"));
}

#[tokio::test]
async fn raising_a_rate_limit_keeps_what_clients_have_already_spent() {
    let gateway = Gateway::new("rate-limit-reload");
    let port = free_port();
    let config = |max: u32| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max={max} window="1h"
        respond "allowed"
    }}
}}
"#
        )
    };

    gateway.apply(&config(2)).await;
    let address = gateway.address();
    get(address, "/").await;
    get(address, "/").await;
    assert!(get(address, "/").await.starts_with("HTTP/1.1 429"));

    // A bigger allowance is still the same client being counted. The bucket
    // carries over at zero and refills toward the new ceiling, so this client
    // stays throttled — a reload does not hand everyone a fresh allowance.
    gateway.apply(&config(3)).await;
    assert!(
        get(address, "/").await.starts_with("HTTP/1.1 429"),
        "reloading must not reset what a client has already spent"
    );
    assert_eq!(
        gateway.store.status().generation.number(),
        2,
        "the reload did happen"
    );
}

#[tokio::test]
async fn changing_what_is_counted_starts_a_new_count() {
    let gateway = Gateway::new("rate-limit-rekey");
    let port = free_port();
    let config = |key: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        rate-limit max=1 window="1h" key="{key}"
        respond "allowed"
    }}
}}
"#
        )
    };

    gateway.apply(&config("ip")).await;
    let address = gateway.address();
    assert!(get(address, "/").await.contains("allowed"));
    assert!(get(address, "/").await.starts_with("HTTP/1.1 429"));

    // Counting per route rather than per client is a different question, so
    // the old answer does not carry over.
    gateway.apply(&config("route")).await;
    assert!(
        get(address, "/").await.contains("allowed"),
        "a different key means a fresh count"
    );
}

#[tokio::test]
async fn an_ip_filter_refuses_an_address_outside_its_allow_list() {
    let gateway = Gateway::new("ip-filter");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/open" {{ respond "open" }}
    route "/closed" {{
        ip-filter allow="10.0.0.0/8"
        respond "secret"
    }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    assert!(get(address, "/open").await.contains("open"));

    // The test client is on loopback, which the allow list does not name.
    let refused = get(address, "/closed").await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(
        !refused.contains("10.0.0.0"),
        "the filter's shape is not the client's business"
    );
}

#[tokio::test]
async fn a_guard_runs_before_the_terminal_whatever_the_file_says() {
    let gateway = Gateway::new("guard-order");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        respond "should never be reached"
        ip-filter deny="0.0.0.0/0"
    }}
}}
"#
        ))
        .await;

    // The filter is written after the terminal; its phase puts it first.
    let response = get(gateway.address(), "/").await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(!response.contains("should never be reached"));
}

#[tokio::test]
async fn a_request_id_is_generated_and_returned() {
    let gateway = Gateway::new("request-id");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    request-id
    route "/*" {{ respond "ok" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let first = get(address, "/").await;
    assert!(first.to_lowercase().contains("x-request-id:"), "{first}");

    let second = get(address, "/").await;
    let id = |response: &str| {
        response
            .lines()
            .find(|line| line.to_lowercase().starts_with("x-request-id:"))
            .map(|line| line.to_string())
            .unwrap_or_default()
    };
    assert_ne!(id(&first), id(&second), "two requests must not share an id");
}

#[tokio::test]
async fn a_client_supplied_request_id_is_ignored_by_default() {
    let gateway = Gateway::new("request-id-untrusted");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    request-id
    route "/*" {{ respond "ok" }}
}}
"#
        ))
        .await;

    let response = request(
        gateway.address(),
        "GET / HTTP/1.1\r\nHost: example.com\r\nX-Request-Id: chosen-by-the-client\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        !response.contains("chosen-by-the-client"),
        "believing a client's id lets one client's traffic be attributed to another: {response}"
    );
}

// ---------------------------------------------------------------------------
// Static files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn it_serves_files_and_refuses_to_leave_the_root() {
    let gateway = Gateway::new("files");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(root.join("assets/app.js"), "console.log(1)").unwrap();
    std::fs::write(root.join(".env"), "SECRET=1").unwrap();
    // A file next to the root, reachable only by escaping it.
    std::fs::write(gateway.directory.join("outside.txt"), "not yours").unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        file-server root="{}"
    }}
}}
"#,
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let address = gateway.address();

    let index = get(address, "/").await;
    assert!(index.contains("<h1>home</h1>"), "{index}");
    assert!(
        index.contains("text/html"),
        "the content type must be set: {index}"
    );

    let script = get(address, "/assets/app.js").await;
    assert!(script.contains("console.log(1)"));
    assert!(script.contains("text/javascript"));

    for attempt in [
        "/../outside.txt",
        "/%2e%2e/outside.txt",
        "/assets/../../outside.txt",
    ] {
        let response = get(address, attempt).await;
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "`{attempt}` escaped the root: {response}"
        );
        assert!(!response.contains("not yours"));
    }

    let dotfile = get(address, "/.env").await;
    assert!(
        dotfile.starts_with("HTTP/1.1 404"),
        "a dotfile must not be served: {dotfile}"
    );
}

#[tokio::test]
async fn an_unchanged_file_is_answered_with_not_modified() {
    let gateway = Gateway::new("files-etag");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.html"), "<p>hi</p>").unwrap();

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" }} }}\n",
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let address = gateway.address();
    let first = get(address, "/page.html").await;
    let tag = first
        .lines()
        .find(|line| line.to_lowercase().starts_with("etag:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
        .expect("a file response carries an etag");

    let second = request(
        address,
        &format!(
            "GET /page.html HTTP/1.1\r\nHost: example.com\r\nIf-None-Match: {tag}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(second.starts_with("HTTP/1.1 304"), "{second}");
    assert!(!second.contains("<p>hi</p>"), "a 304 must not carry the body");
}

#[tokio::test]
async fn a_directory_without_an_index_is_not_listed_unless_asked() {
    let gateway = Gateway::new("files-browse");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(root.join("files")).unwrap();
    std::fs::write(root.join("files/a.txt"), "a").unwrap();
    let root_argument = root.display().to_string().replace('\\', "/");

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{root_argument}\" }} }}\n"
        ))
        .await;
    let address = gateway.address();
    assert!(get(address, "/files/").await.starts_with("HTTP/1.1 404"));

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{root_argument}\" browse=#true }} }}\n"
        ))
        .await;
    let listing = get(address, "/files/").await;
    assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
    assert!(listing.contains("a.txt"));
}

#[tokio::test]
async fn a_missing_root_stops_the_reload_rather_than_404ing_everything() {
    let gateway = Gateway::new("files-missing");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.html"), "home").unwrap();
    let good = root.display().to_string().replace('\\', "/");

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{good}\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.store.status().generation.number(), 1);

    let outcome = gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{good}/absent\" }} }}\n"
        ))
        .await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert!(
        get(gateway.address(), "/").await.contains("home"),
        "the previous generation still serves"
    );
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// Read a response and split it into the head and the raw body bytes, because
/// a compressed body is not text.
async fn raw(address: SocketAddr, path: &str, accept: &str) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: {accept}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(Ok(read)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut chunk)).await
    {
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let split = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(buffer.len());
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let body = buffer[split..].to_vec();
    // A compressed response has no length until it ends, so it arrives chunked.
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(&body)
    } else {
        body
    };
    (head, body)
}

/// Undo chunked framing, so a test can look at the bytes the client sees.
fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            return out;
        };
        let size = std::str::from_utf8(&rest[..line_end])
            .ok()
            .and_then(|text| usize::from_str_radix(text.trim(), 16).ok());
        let Some(size) = size else { return out };
        if size == 0 {
            return out;
        }
        let start = line_end + 2;
        let end = start + size;
        if end > rest.len() {
            return out;
        }
        out.extend_from_slice(&rest[start..end]);
        // Skip the CRLF that closes the chunk.
        rest = &rest[(end + 2).min(rest.len())..];
    }
}

#[tokio::test]
async fn a_large_text_response_is_compressed_for_a_client_that_accepts_it() {
    let gateway = Gateway::new("encode");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    // Highly compressible, and well over the threshold.
    let original = "compress me. ".repeat(500);
    std::fs::write(root.join("page.html"), &original).unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        encode "gzip"
        file-server root="{}"
    }}
}}
"#,
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let address = gateway.address();
    let (head, body) = raw(address, "/page.html", "gzip").await;
    assert!(head.to_lowercase().contains("content-encoding: gzip"), "{head}");
    assert!(head.to_lowercase().contains("vary: accept-encoding"), "{head}");
    assert!(
        !head.to_lowercase().contains("content-length:"),
        "the length is not known until the body ends: {head}"
    );
    assert!(
        body.len() < original.len() / 2,
        "compressed {} of {}",
        body.len(),
        original.len()
    );
    assert_eq!(
        &body[..2],
        &[0x1f, 0x8b],
        "a gzip member starts with its magic bytes"
    );
}

#[tokio::test]
async fn a_client_that_does_not_accept_compression_gets_the_original() {
    let gateway = Gateway::new("encode-plain");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    let original = "plain text. ".repeat(500);
    std::fs::write(root.join("page.html"), &original).unwrap();

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ encode \"gzip\"\n file-server root=\"{}\" }} }}\n",
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let (head, body) = raw(gateway.address(), "/page.html", "identity").await;
    assert!(!head.to_lowercase().contains("content-encoding:"), "{head}");
    // Vary is still set: a cache must not serve this copy to a client that
    // would have received a compressed one.
    assert!(head.to_lowercase().contains("vary: accept-encoding"), "{head}");
    assert_eq!(String::from_utf8_lossy(&body), original);
}

#[tokio::test]
async fn a_small_response_is_not_worth_compressing() {
    let gateway = Gateway::new("encode-small");
    let port = free_port();
    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        encode "gzip" min-size="1KiB"
        respond "short"
    }}
}}
"#
        ))
        .await;

    let (head, body) = raw(gateway.address(), "/", "gzip").await;
    assert!(!head.to_lowercase().contains("content-encoding:"), "{head}");
    assert_eq!(String::from_utf8_lossy(&body), "short");
}

#[tokio::test]
async fn a_file_server_answers_a_range_request() {
    let gateway = Gateway::new("file-range");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("data.bin"), b"0123456789").unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="files" {{ file-server root="www" }}
}}
"#
        ))
        .await;

    let address = gateway.address();
    let whole = get(address, "/data.bin").await;
    assert!(whole.contains("accept-ranges: bytes"), "{whole}");
    assert!(whole.ends_with("0123456789"), "{whole}");

    // What a resumed download sends.
    let part = request(
        address,
        "GET /data.bin HTTP/1.1\r\nHost: h\r\nRange: bytes=2-5\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(part.starts_with("HTTP/1.1 206"), "{part}");
    assert!(part.contains("content-range: bytes 2-5/10"), "{part}");
    assert!(part.contains("content-length: 4"), "{part}");
    assert!(part.ends_with("2345"), "{part}");

    // Past the end: told how long the file is, so the next ask can be sane.
    let past = request(
        address,
        "GET /data.bin HTTP/1.1\r\nHost: h\r\nRange: bytes=50-60\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(past.starts_with("HTTP/1.1 416"), "{past}");
    assert!(past.contains("content-range: bytes */10"), "{past}");
}

#[tokio::test]
async fn a_large_file_is_not_read_into_memory_to_send_it() {
    let gateway = Gateway::new("file-large");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    // Bigger than the inline threshold, so it has to be streamed. A gateway
    // that buffered every download could be emptied of memory by a few of them.
    let size = 200 * 1024;
    std::fs::write(root.join("big.bin"), vec![b'x'; size]).unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="files" {{ file-server root="www" }}
}}
"#
        ))
        .await;

    let response = get(gateway.address(), "/big.bin").await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "{}",
        &response[..80.min(response.len())]
    );
    assert!(
        response.contains(&format!("content-length: {size}")),
        "the length is still declared: {}",
        &response[..200.min(response.len())]
    );
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    assert_eq!(body.len(), size, "every byte arrived");
}

#[tokio::test]
async fn a_single_page_application_is_served_from_one_directory() {
    // The shape every React, Vue or Svelte deployment has: the application's
    // router owns most paths, and the server has to hand back the shell for
    // them rather than a 404. Without this, hosting a SPA is not possible at
    // all, which is why `try_files` is in every nginx configuration on the
    // internet.
    let gateway = Gateway::new("files-spa");
    let port = free_port();
    let root = gateway.directory.join("app");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("index.html"), "<html>shell</html>").unwrap();
    std::fs::write(root.join("assets/app.js"), "console.log(1)").unwrap();

    gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{
    route "/*" {{
        file-server root="{}" fallback="/index.html"
    }}
}}
"#,
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let address = gateway.address();

    // A route the application owns.
    let deep = get(address, "/users/42/settings").await;
    assert!(deep.starts_with("HTTP/1.1 200"), "{deep}");
    assert!(deep.contains("shell"), "{deep}");
    assert!(
        deep.to_lowercase().contains("content-type: text/html"),
        "the shell is HTML whatever path asked for it:\n{deep}"
    );

    // A real file still wins.
    let asset = get(address, "/assets/app.js").await;
    assert!(asset.contains("console.log(1)"), "{asset}");
    assert!(
        asset.to_lowercase().contains("text/javascript"),
        "a real file keeps its own type:\n{asset}"
    );

    // And the root itself is the shell by way of the index, not the fallback.
    assert!(get(address, "/").await.contains("shell"));
}

#[tokio::test]
async fn without_a_fallback_a_missing_file_is_still_a_404() {
    let gateway = Gateway::new("files-no-fallback");
    let port = free_port();
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.html"), "<html>home</html>").unwrap();

    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" }} }}\n",
            root.display().to_string().replace('\\', "/")
        ))
        .await;

    let response = get(gateway.address(), "/users/42").await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}
