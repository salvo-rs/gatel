//! Representation metadata and auxiliary-file containment through real HTTP.
mod support;
use http_body_util::BodyExt;
use support::*;
use tokio::io::AsyncReadExt;

fn field<'a>(response: &'a str, name: &str) -> &'a str {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .unwrap()
        .1
        .trim()
}

#[tokio::test]
async fn mixed_case_extensions_keep_media_type_and_dynamic_encoding_eligibility() {
    let gateway = Gateway::new("file-extension-case");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    for name in [
        "page.HTML",
        "style.CsS",
        "code.Js",
        "data.JsOn",
        "note.TXT",
        "image.SVG",
        "unknown.XYZ",
    ] {
        std::fs::write(root.join(name), "fixture ".repeat(200)).unwrap();
    }
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ encode \"gzip\" min-size=1; file-server root=\"{}\"; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for (name, media) in [
        ("page.HTML", "text/html; charset=utf-8"),
        ("style.CsS", "text/css; charset=utf-8"),
        ("code.Js", "text/javascript; charset=utf-8"),
        ("data.JsOn", "application/json"),
        ("note.TXT", "text/plain; charset=utf-8"),
        ("image.SVG", "image/svg+xml"),
    ] {
        let identity = get(gateway.address(), &format!("/{name}")).await;
        assert!(identity.starts_with("HTTP/1.1 200"), "{identity}");
        assert_eq!(field(&identity, "content-type"), media, "{name}");
        let encoded = request(gateway.address(), &format!("HEAD /{name} HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n")).await;
        assert_eq!(field(&encoded, "content-type"), media);
        assert_eq!(field(&encoded, "content-encoding"), "gzip");
        assert!(encoded.ends_with("\r\n\r\n"));
        // Decode HTTP framing independently before decompressing the actual
        // representation. The text-only request helper cannot verify gzip bytes.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let io = hyper_util::rt::TokioIo::new(
                tokio::net::TcpStream::connect(gateway.address()).await.unwrap(),
            );
            let (mut client, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
            let driver = tokio::spawn(connection);
            let response = client
                .send_request(
                    http::Request::builder()
                        .uri(format!("/{name}"))
                        .header("host", "example.com")
                        .header("accept-encoding", "gzip")
                        .header("connection", "close")
                        .body(http_body_util::Empty::<bytes::Bytes>::new())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], media);
            assert_eq!(response.headers()["content-encoding"], "gzip");
            assert_eq!(response.headers()["etag"], field(&encoded, "etag"));
            assert_eq!(response.headers()["vary"], field(&encoded, "vary"));
            let compressed = response.into_body().collect().await.unwrap().to_bytes();
            let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(compressed.as_ref());
            let mut decoded = Vec::new();
            decoder.read_to_end(&mut decoded).await.unwrap();
            assert_eq!(decoded, "fixture ".repeat(200).as_bytes(), "{name}");
            drop(client);
            driver.await.unwrap().unwrap();
        })
        .await
        .expect("gzip GET did not finish");
    }
    let unknown = get(gateway.address(), "/unknown.XYZ").await;
    assert_eq!(field(&unknown, "content-type"), "application/octet-stream");
}

#[tokio::test]
async fn stale_sidecars_are_excluded_before_encoding_and_validator_selection() {
    let gateway = Gateway::new("file-variant-freshness");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    let stamp = |name: &str, seconds| {
        std::fs::File::options()
            .write(true)
            .open(root.join(name))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .unwrap();
    };
    for (name, body) in [
        ("page.txt", "identity"),
        ("page.txt.br", "brotli"),
        ("page.txt.zst", "zstd"),
        ("page.txt.gz", "gzip"),
    ] {
        std::fs::write(root.join(name), body).unwrap();
        stamp(name, 1_700_000_000);
    }
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" precompressed=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    // Equal timestamps are accepted for every supported coding.
    for coding in ["br", "zstd", "gzip"] {
        let response = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: {coding}\r\nConnection: close\r\n\r\n")).await;
        assert_eq!(field(&response, "content-encoding"), coding);
    }
    let old = request(
        gateway.address(),
        "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
    )
    .await;
    std::fs::write(root.join("page.txt"), "updated identity").unwrap();
    stamp("page.txt", 1_700_000_010);
    for method in ["GET", "HEAD"] {
        let response = request(gateway.address(), &format!("{method} /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: br, zstd, gzip\r\nIf-None-Match: {}\r\nConnection: close\r\n\r\n", field(&old, "etag"))).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            !response.to_ascii_lowercase().contains("content-encoding:"),
            "{response}"
        );
        assert_eq!(field(&response, "vary"), "accept-encoding");
        assert_eq!(field(&response, "content-length"), "16");
        assert!(response.ends_with(if method == "GET" {
            "updated identity"
        } else {
            "\r\n\r\n"
        }));
    }
    // Stale high-preference codings must not suppress a fresh lower one.
    stamp("page.txt.gz", 1_700_000_020);
    let response = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: br, zstd;q=0.9, gzip;q=0.8, identity;q=0.1\r\nConnection: close\r\n\r\n").await;
    assert_eq!(field(&response, "content-encoding"), "gzip");
    assert!(response.ends_with("gzip"));
}

#[tokio::test]
async fn precompressed_validators_and_head_describe_the_selected_file() {
    let gateway = Gateway::new("file-variant-metadata");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "identity").unwrap();
    // The file server treats a precompressed sibling as opaque bytes; this
    // fixture tests selection/metadata, not the compression codec.
    std::fs::write(root.join("page.txt.gz"), "coded").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" precompressed=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let identity = get(gateway.address(), "/page.txt").await;
    let identity_tag = field(&identity, "etag");
    let encoded = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nIf-None-Match: {identity_tag}\r\nConnection: close\r\n\r\n")).await;
    assert!(
        encoded.starts_with("HTTP/1.1 200") && encoded.ends_with("coded"),
        "{encoded}"
    );
    let encoded_tag = field(&encoded, "etag");
    assert_ne!(identity_tag, encoded_tag);
    for (accept, expected) in [("identity", &identity), ("gzip", &encoded)] {
        let head = request(gateway.address(), &format!("HEAD /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: {accept}\r\nConnection: close\r\n\r\n")).await;
        assert!(
            head.starts_with("HTTP/1.1 200") && head.ends_with("\r\n\r\n"),
            "{head}"
        );
        for name in ["etag", "last-modified", "content-length", "vary"] {
            assert_eq!(field(&head, name), field(expected, name), "{name}: {head}");
        }
    }
    let validated = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nIf-None-Match: {encoded_tag}\r\nConnection: close\r\n\r\n")).await;
    assert!(
        validated.starts_with("HTTP/1.1 304") && validated.ends_with("\r\n\r\n"),
        "{validated}"
    );
    assert_eq!(field(&validated, "vary"), "accept-encoding");
    assert_eq!(field(&validated, "content-encoding"), "gzip");
    std::fs::write(root.join("page.txt.gz"), "new coded representation").unwrap();
    let changed = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\nIf-None-Match: {encoded_tag}\r\nConnection: close\r\n\r\n")).await;
    assert!(
        changed.starts_with("HTTP/1.1 200") && changed.ends_with("new coded representation"),
        "{changed}"
    );
    assert_ne!(field(&changed, "etag"), encoded_tag);
}

#[tokio::test]
async fn directory_index_outside_the_root_is_not_served() {
    let gateway = Gateway::new("file-index-containment");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    let outside = gateway.directory.join("secret.html");
    std::fs::write(&outside, "outside secret").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\"{}\" index=\"{}\"; }} }}", root.display().to_string().replace('\\', "/"), outside.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    let response = get(gateway.address(), "/").await;
    assert!(
        response.starts_with("HTTP/1.1 404") && !response.contains("outside secret"),
        "{response}"
    );
}

#[tokio::test]
async fn file_negotiation_coordinates_dynamic_encoding_preconditions_and_cache() {
    let gateway = Gateway::new("file-negotiation-boundary");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "negotiated source ".repeat(100)).unwrap();
    let port = free_port();
    for cache in ["", "cache default-ttl=\"60s\";"] {
        for encode in ["", "encode \"gzip\" min-size=\"0B\";"] {
            let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ {cache} {encode} file-server root=\"{}\" precompressed=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
            assert!(
                !matches!(outcome, gatel_plan::Outcome::Rejected { .. }),
                "{outcome:?}"
            );
            // Warm identity first: it must not satisfy an identity refusal.
            let identity = get(gateway.address(), "/page.txt").await;
            assert!(identity.starts_with("HTTP/1.1 200"));
            for repeat in 0..2 {
                let get = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nConnection: close\r\n\r\n").await;
                let head = request(gateway.address(), "HEAD /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nConnection: close\r\n\r\n").await;
                for response in [&get, &head] {
                    assert_eq!(field(response, "vary"), "accept-encoding");
                    if encode.is_empty() {
                        assert!(response.starts_with("HTTP/1.1 406"), "{response}");
                        assert_eq!(field(response, "cache-control"), "no-store");
                        assert!(!response.to_lowercase().contains("etag:"));
                        assert!(response.ends_with("\r\n\r\n"));
                    } else {
                        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
                        assert_eq!(field(response, "content-encoding"), "gzip");
                        assert!(field(response, "etag").starts_with("W/\""));
                    }
                }
                assert!(head.ends_with("\r\n\r\n"));
                if !encode.is_empty() {
                    assert_eq!(field(&get, "etag"), field(&head, "etag"));
                    assert!(!head.to_lowercase().contains("content-length:"), "{head}");
                    let validated = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nIf-None-Match: {}\r\nConnection: close\r\n\r\n", field(&get, "etag"))).await;
                    assert!(validated.starts_with("HTTP/1.1 304"), "{validated}");
                    assert_eq!(field(&validated, "content-encoding"), "gzip");
                    if repeat == 1 && !cache.is_empty() {
                        assert!(get.contains("hit;"), "{get}");
                    }
                }
            }
            for extra in ["", "Range: bytes=0-2\r\n", "If-None-Match: *\r\n"] {
                let refused = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: *;q=0\r\n{extra}Connection: close\r\n\r\n")).await;
                assert!(refused.starts_with("HTTP/1.1 406"), "{refused}");
            }
            if !encode.is_empty() {
                let ranged = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nRange: bytes=0-2\r\nConnection: close\r\n\r\n").await;
                assert!(ranged.starts_with("HTTP/1.1 200"), "{ranged}");
                assert_eq!(field(&ranged, "content-encoding"), "gzip");
                assert!(!ranged.to_lowercase().contains("content-range:"));
                let refused = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nCache-Control: no-transform, no-cache\r\nConnection: close\r\n\r\n").await;
                assert!(refused.starts_with("HTTP/1.1 406"), "{refused}");
            }
        }
    }
}

#[tokio::test]
async fn static_and_dynamic_offers_share_quality_and_eligibility_rules() {
    let gateway = Gateway::new("file-combined-offers");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    for name in ["page.txt", "tiny.txt", "image.png"] {
        std::fs::write(
            root.join(name),
            if name == "tiny.txt" {
                "x".into()
            } else {
                "data".repeat(500)
            },
        )
        .unwrap();
    }
    std::fs::write(root.join("page.txt.br"), "precompressed brotli").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ encode \"gzip\" min-size=\"1KiB\"; file-server root=\"{}\" precompressed=#true; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for (accept, expected) in [("gzip, br;q=0.5", "gzip"), ("br, gzip;q=0.5", "br")] {
        let response = request(gateway.address(), &format!("GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: {accept}\r\nConnection: close\r\n\r\n")).await;
        assert_eq!(field(&response, "content-encoding"), expected);
    }
    for path in ["/tiny.txt", "/image.png"] {
        for method in ["GET", "HEAD"] {
            let response = request(gateway.address(), &format!("{method} {path} HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nConnection: close\r\n\r\n")).await;
            assert!(response.starts_with("HTTP/1.1 406"), "{response}");
        }
    }
    // Selecting an existing coded file is not a dynamic transformation.
    let response = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: br, identity;q=0\r\nCache-Control: no-transform\r\nConnection: close\r\n\r\n").await;
    assert_eq!(field(&response, "content-encoding"), "br");
    assert!(response.ends_with("precompressed brotli"));
    // Updating the source retires br but does not retire dynamic gzip.
    std::fs::File::options()
        .write(true)
        .open(root.join("page.txt.br"))
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        )
        .unwrap();
    let response = request(gateway.address(), "GET /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: br, gzip;q=0.5, identity;q=0\r\nConnection: close\r\n\r\n").await;
    assert_eq!(field(&response, "content-encoding"), "gzip");
}

#[tokio::test]
async fn configured_response_policy_is_known_before_file_negotiation() {
    let gateway = Gateway::new("file-header-policy");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "policy".repeat(500)).unwrap();
    let port = free_port();
    for (site, route, status) in [
        ("", "set \"cache-control\" \"no-transform\";", "406"),
        ("", "set \"content-type\" \"image/png\";", "406"),
        // Site response operations run last, so they can remove an inner ban.
        (
            "headers { response { remove \"cache-control\"; } }",
            "set \"cache-control\" \"no-transform\";",
            "200",
        ),
        (
            "headers { response { set \"cache-control\" \"no-transform\"; } }",
            "remove \"cache-control\";",
            "406",
        ),
    ] {
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ {site}\n route \"/*\" {{ encode \"gzip\"; headers {{ response {{ {route} }} }}; file-server root=\"{}\"; }} }}", root.display().to_string().replace('\\', "/"))).await;
        assert!(
            matches!(
                outcome,
                gatel_plan::Outcome::Started { .. } | gatel_plan::Outcome::Reloaded { .. }
            ),
            "{outcome:?}"
        );
        for method in ["GET", "HEAD"] {
            let response = request(gateway.address(), &format!("{method} /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip, identity;q=0\r\nConnection: close\r\n\r\n")).await;
            assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
            if status == "200" {
                assert_eq!(field(&response, "content-encoding"), "gzip");
            }
        }
    }
}

#[tokio::test]
async fn custom_negotiation_error_page_replaces_zero_length_framing() {
    let gateway = Gateway::new("file-negotiation-error-page");
    let root = gateway.directory.join("www");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("page.txt"), "private original resource").unwrap();
    let port = free_port();
    let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ cache default-ttl=\"60s\"; error-pages {{ page 406 body=\"No suitable representation\"; }}; file-server root=\"{}\"; }} }}", root.display().to_string().replace('\\', "/"))).await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    for method in ["GET", "HEAD", "GET"] {
        let response = request(gateway.address(), &format!("{method} /page.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: *;q=0\r\nConnection: close\r\n\r\n")).await;
        assert!(response.starts_with("HTTP/1.1 406"), "{response}");
        assert_eq!(field(&response, "content-length"), "26");
        assert_eq!(field(&response, "vary"), "accept-encoding");
        assert_eq!(field(&response, "cache-control"), "no-store");
        assert!(!response.contains("private original resource"));
        assert!(response.ends_with(if method == "HEAD" {
            "\r\n\r\n"
        } else {
            "No suitable representation"
        }));
    }
}
