#[cfg(windows)]
mod support;

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    if std::env::var_os("CGI_CHILD").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::process::exit(0);
    }
    if let Ok(method) = std::env::var("REQUEST_METHOD") {
        use std::io::{Read, Write};
        let query = std::env::var("QUERY_STRING").unwrap_or_default();
        if query == "limit" {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .env("CGI_CHILD", "1")
                .creation_flags(0x08000000);
            let _child = command.spawn().unwrap();
            assert!(command.spawn().is_err(), "configured Job limit was not enforced");
            print!("Content-Type: text/plain\r\nContent-Length: 0\r\n\r\n");
            std::io::stdout().flush().unwrap();
            std::process::exit(0);
        }
        if query == "early" {
            std::process::exit(7);
        }
        if query == "hang" || query == "fail" {
            print!("Content-Type: text/plain\r\nContent-Length: 0\r\n\r\n");
            std::io::stdout().flush().unwrap();
            if query == "hang" {
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
            std::process::exit(7);
        }
        let mut body = Vec::new();
        std::io::stdin().read_to_end(&mut body).unwrap();
        if body.is_empty() {
            body.extend_from_slice(b"hello");
        }
        print!(
            "Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        if method != "HEAD" {
            std::io::stdout().write_all(&body).unwrap();
        }
        std::process::exit(0);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(20), verify())
                .await
                .unwrap();
        });
}

#[cfg(windows)]
async fn verify() {
    use support::*;
    let mut gateway = Gateway::new("cgi-process");
    gateway.registry = gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(gatel_cgi::bundle())
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("app.marker"), "fixture").unwrap();
    let executable = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let port = free_port();
    let config = |program: &str| {
        format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ cgi "{program}" root="." script="app.marker" script-name="/app" concurrency=1 process-limit=2 timeout=1; }} }}"#
        )
    };
    let result = gateway.apply(&config(&executable)).await;
    assert!(
        matches!(result, gatel_plan::Outcome::Started { .. }),
        "{result:?}"
    );
    for (index, (method, path, body, status)) in [
        ("GET", "/app", "", "200"),
        ("GET", "/app?limit", "", "200"),
        ("POST", "/app", "posted", "200"),
        ("HEAD", "/app", "", "200"),
        ("GET", "/app?fail", "", "502"),
        ("GET", "/app?early", "", "502"),
        ("GET", "/app?hang", "", "504"),
        ("GET", "/app", "", "200"),
    ]
    .into_iter()
    .enumerate()
    {
        let response = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let response = request(gateway.address(), &format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())).await;
                // Timeout can return before process cleanup: admission must
                // remain unavailable until the supervisor verifies the reap.
                if index == 7 && response.starts_with("HTTP/1.1 503") { continue; }
                break response;
            }
        }).await.unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "case {index} {method} {path}: {response}"
        );
        if status == "200" {
            assert!(
                response.ends_with(if method == "HEAD" || path == "/app?limit" {
                    "\r\n\r\n"
                } else if method == "POST" {
                    "posted"
                } else {
                    "hello"
                }),
                "{response}"
            );
        }
    }
    let rejected = gateway.apply(&config("missing.exe")).await;
    for limit in ["0", "1025", "-1", "4294967296", "\"two\""] {
        let invalid = config(&executable).replace("process-limit=2", &format!("process-limit={limit}"));
        let result = gateway.apply(&invalid).await;
        assert!(
            !matches!(
                result,
                gatel_plan::Outcome::Started { .. } | gatel_plan::Outcome::Reloaded { .. }
            ),
            "{result:?}"
        );
        assert!(get(gateway.address(), "/app").await.ends_with("hello"));
    }
    for limit in [1, 1024, 2] {
        let changed = config(&executable).replace("process-limit=2", &format!("process-limit={limit}"));
        assert!(matches!(
            gateway.apply(&changed).await,
            gatel_plan::Outcome::Reloaded { .. }
        ));
        assert!(get(gateway.address(), "/app").await.ends_with("hello"));
    }
    assert!(
        get(gateway.address(), "/app?limit")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        !matches!(
            rejected,
            gatel_plan::Outcome::Started { .. } | gatel_plan::Outcome::Reloaded { .. }
        ),
        "{rejected:?}"
    );
    assert!(get(gateway.address(), "/app").await.ends_with("hello"));
    let replacement = gateway
        .apply(&format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ respond "replacement"; }} }}"#
        ))
        .await;
    assert!(
        matches!(replacement, gatel_plan::Outcome::Reloaded { .. }),
        "{replacement:?}"
    );
    assert!(get(gateway.address(), "/app").await.ends_with("replacement"));
    gateway.stop().await;
}
