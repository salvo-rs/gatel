//! The standard directives, end to end.

mod support;

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
