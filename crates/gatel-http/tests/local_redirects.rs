//! Automatic canonicalization must never emit a network-path redirect.
mod support;
use support::*;

#[tokio::test]
async fn automatic_redirects_preserve_queries_without_emitting_an_authority() {
    for mode in ["directory", "add", "remove"] {
        let gateway = Gateway::new("local-redirects");
        let root = gateway.directory.join("www");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("a b")).unwrap();
        std::fs::write(root.join("sub").join("index.html"), "subdirectory").unwrap();
        let port = free_port();
        let directive = if mode == "directory" {
            format!(
                "file-server root=\"{}\"",
                root.display().to_string().replace('\\', "/")
            )
        } else {
            format!("trailing-slash \"{mode}\"; respond \"body\"")
        };
        let outcome = gateway
            .apply(&format!(
                "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/*\" {{ {directive}; }} }}"
            ))
            .await;
        assert!(
            matches!(outcome, gatel_plan::Outcome::Started { .. }),
            "{outcome:?}"
        );
        let cases = if mode == "remove" {
            vec![
                ("//sub/?a=1&a=2", "/sub?a=1&a=2"),
                ("///sub/?next=%2Fdocs%3Fx%3D1", "/sub?next=%2Fdocs%3Fx%3D1"),
                ("////?x=1", "/?x=1"),
            ]
        } else {
            vec![
                ("//sub?a=1&a=2", "/sub/?a=1&a=2"),
                ("///sub?next=%2Fdocs%3Fx%3D1", "/sub/?next=%2Fdocs%3Fx%3D1"),
                ("/a%20b?x=1", "/a%20b/?x=1"),
            ]
        };
        for (path, expected) in cases {
            let response = get(gateway.address(), path).await;
            let status = if mode == "directory" { 301 } else { 308 };
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}")),
                "{mode} {path}: {response}"
            );
            let location = response
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .unwrap()
                .1
                .trim();
            assert_eq!(location, expected, "{mode} {path}");
            assert!(!location.starts_with("//") && !location.contains('\\'));
        }
    }
}
