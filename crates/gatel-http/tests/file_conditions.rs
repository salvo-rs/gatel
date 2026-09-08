//! Preconditions are evaluated before ranges and shared with cached responses.
mod support;
use support::*;

#[tokio::test]
async fn file_precondition_priority_and_weak_comparison_on_the_wire() {
    for cached in [false, true] {
        let gateway = Gateway::new("file-preconditions");
        let root = gateway.directory.join("www");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("page.txt"), "representation").unwrap();
        let port = free_port();
        let cache = if cached { "cache default-ttl=\"60s\";" } else { "" };
        let outcome = gateway.apply(&format!("global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ {cache} file-server root=\"{}\"; }} }}", root.display().to_string().replace('\\', "/"))).await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{outcome:?}"
        );
        let original = get(gateway.address(), "/page.txt").await;
        let field = |name: &str| {
            original
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .unwrap()
                .1
                .trim()
        };
        let tag = field("etag");
        let date = field("last-modified");
        let old = "Thu, 01 Jan 1970 00:00:00 GMT";
        for (method, headers, status) in [
            ("GET", format!("If-Match: {tag}\r\n"), 200),
            ("GET", format!("If-Match: W/{tag}\r\n"), 412),
            ("GET", "If-Match: \"missing\"\r\n".into(), 412),
            (
                "GET",
                format!("If-Match: {tag}\r\nIf-Unmodified-Since: {old}\r\n"),
                200,
            ),
            (
                "GET",
                format!("If-Unmodified-Since: {old}\r\nIf-None-Match: {tag}\r\n"),
                412,
            ),
            ("GET", format!("If-Unmodified-Since: {date}\r\n"), 200),
            ("GET", format!("If-None-Match: W/{tag}\r\n"), 304),
            ("HEAD", format!("If-None-Match: {tag}\r\n"), 304),
            ("HEAD", format!("If-Match: W/{tag}\r\n"), 412),
            (
                "GET",
                format!("If-None-Match: \"missing\"\r\nIf-Modified-Since: {date}\r\n"),
                200,
            ),
            (
                "GET",
                format!("If-None-Match: \"other,tag\"\r\nIf-None-Match: {tag}\r\n"),
                304,
            ),
            (
                "GET",
                format!("If-None-Match: *\r\nIf-None-Match: {tag}\r\n"),
                400,
            ),
            ("GET", format!("If-None-Match: {tag}, invalid\r\n"), 400),
            (
                "GET",
                format!("If-Modified-Since: {date}\r\nRange: bytes=0-3\r\n"),
                304,
            ),
            ("GET", format!("If-Modified-Since: {old}\r\n"), 200),
            ("GET", "If-Modified-Since: invalid\r\n".into(), 200),
            (
                "GET",
                format!("If-Modified-Since: {date}\r\nIf-Modified-Since: {date}\r\n"),
                200,
            ),
            ("GET", format!("If-Match: {tag}\r\nRange: bytes=0-3\r\n"), 206),
            ("GET", format!("If-Range: {tag}\r\nRange: BYTES=0-3\r\n"), 206),
            (
                "GET",
                format!("If-Range: {tag}\r\nIf-Range: {tag}\r\nRange: bytes=0-3\r\n"),
                200,
            ),
            ("GET", format!("If-Range: W/{tag}\r\nRange: bytes=0-3\r\n"), 200),
            ("GET", "Range: bytes=0-3\r\nRange: bytes=5-8\r\n".into(), 200),
            ("GET", "Range: bytes=+0-3\r\n".into(), 200),
            ("GET", "Range: bytes=8-3\r\n".into(), 200),
        ] {
            let response = request(
                gateway.address(),
                &format!(
                    "{method} /page.txt HTTP/1.1\r\nHost: example.com\r\n{headers}Connection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}")),
                "cache={cached} {headers}: {response}"
            );
            if status == 304 || method == "HEAD" {
                assert!(response.ends_with("\r\n\r\n"), "{response}");
            }
            if matches!(status, 400 | 412) && method != "HEAD" {
                assert!(
                    response.ends_with("file precondition rejected\n") && response.contains("no-store"),
                    "{response}"
                );
            }
            if status == 200 && method == "GET" {
                assert!(response.ends_with("representation"), "{response}");
            }
            if status == 206 {
                assert!(response.ends_with("repr"), "{response}");
            }
        }
        let missing = request(
            gateway.address(),
            "GET /absent HTTP/1.1\r\nHost: example.com\r\nIf-Match: *\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    }
}
