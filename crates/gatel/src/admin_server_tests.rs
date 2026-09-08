use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::admin::tests::{TOKEN, setup};

async fn request(address: SocketAddr, text: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(text.as_bytes()).await.unwrap();
        let mut output = String::new();
        socket.read_to_string(&mut output).await.unwrap();
        output
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn slow_writes_cannot_consume_the_diagnostic_read_reserve() {
    tokio::time::timeout(Duration::from_secs(4), async {
        let (app, api) = setup().await;
        let api = Arc::new(api);
        let server = AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), api.clone()).unwrap();
        let version = app.configuration_status().version();
        let tag = version.etag();
        let mut writers = Vec::new();
        for _ in 0..8 {
            let mut socket = tokio::net::TcpStream::connect(server.address()).await.unwrap();
            socket.write_all(format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nContent-Length: 100\r\n\r\n").as_bytes()).await.unwrap();
            writers.push(socket);
        }
        while api.requests.available_permits() != 0 { tokio::task::yield_now().await; }
        for path in ["/v1/config/version", "/v1/config/status"] {
            let response = request(server.address(), &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert_eq!(api.requests.available_permits(), 0, "slow writes must still be pending");
        }
        let ninth = request(server.address(), &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nContent-Length: 100\r\n\r\n")).await;
        assert!(ninth.starts_with("HTTP/1.1 503"), "{ninth}");
        // Reads are bounded too, not an unbounded alternate admission path.
        let reads = api.read_requests.acquire_many(2).await.unwrap();
        let exhausted = request(server.address(), &format!("GET /v1/config/status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")).await;
        assert!(exhausted.starts_with("HTTP/1.1 503"), "{exhausted}");
        drop(reads);
        assert_eq!(app.configuration_status().version(), version);
        assert!(app.configuration_status().last_failure.is_none());
        server.shutdown().await.unwrap();
        assert_eq!(api.requests.available_permits(), 8);
        assert_eq!(api.read_requests.available_permits(), 2);
        drop(writers);
        app.shutdown().await;
    }).await.unwrap();
}

#[tokio::test]
async fn real_http_version_put_and_chunked_overflow() {
    let (app, api) = setup().await;
    let server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Arc::new(api)).unwrap();
    let address = server.address();
    let unauth = request(
        address,
        "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(unauth.starts_with("HTTP/1.1 401"));
    assert!(!unauth.contains("etag:"));
    let version = request(
        address,
        &format!(
            "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
        ),
    )
    .await;
    assert!(version.starts_with("HTTP/1.1 200"));
    let tag = version
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    let body = "upstream \"runtime/network\" { endpoint \"127.0.0.1:2\"; }";
    let result = request(address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await;
    assert!(result.starts_with("HTTP/1.1 200"), "{result}");
    assert_eq!(app.published_for_test().unwrap().plan.upstreams.len(), 1);
    let tag = app.configuration_status().version().etag();
    // Chunked framing reaches the real HTTP parser and body limiter without CL.
    let huge = " ".repeat(super::super::MAX_BODY + 1);
    let overflow = request(address, &format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {tag}\r\nContent-Type: application/kdl\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{huge}\r\n0\r\n\r\n", huge.len())).await;
    assert!(overflow.starts_with("HTTP/1.1 413"), "{overflow}");
    assert_eq!(app.configuration_status().version().etag(), tag);
    server.shutdown().await.unwrap();
    app.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_incomplete_headers_and_authenticated_body() {
    let (app, api) = setup().await;
    let api = Arc::new(api);
    let server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), api.clone()).unwrap();
    let mut header = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    header.write_all(b"GET /v1/config").await.unwrap();
    let mut body = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    let version = app.configuration_status().version();
    body.write_all(format!("PUT /v1/config HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: {}\r\nContent-Type: application/kdl\r\nContent-Length: 50\r\n\r\n", version.etag()).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while api.requests.available_permits() == 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(api.requests.available_permits(), 8);
    assert_eq!(app.configuration_status().version(), version);
    assert!(app.configuration_status().last_failure.is_none());
    for mut socket in [header, body] {
        let result = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut [0; 1]))
            .await
            .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
    }
    app.shutdown().await;
}

#[tokio::test]
async fn connection_limit_rejects_overflow_and_shutdown_releases_every_slot() {
    let (app, api) = setup().await;
    let server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Arc::new(api)).unwrap();
    let mut sockets = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        sockets.push(tokio::net::TcpStream::connect(server.address()).await.unwrap());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.active_connections() != MAX_CONNECTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut overflow = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    let read = tokio::time::timeout(Duration::from_secs(1), overflow.read(&mut [0; 1]))
        .await
        .unwrap();
    assert!(matches!(read, Ok(0) | Err(_)));
    let slots = server.slots.clone();
    server.shutdown().await.unwrap();
    assert_eq!(slots.available_permits(), MAX_CONNECTIONS);
    drop(sockets);
    app.shutdown().await;
}

#[tokio::test]
async fn wildcard_plaintext_listener_is_refused() {
    let (app, api) = setup().await;
    let result = AdminServer::start(std::net::TcpListener::bind("0.0.0.0:0").unwrap(), Arc::new(api));
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidInput));
    app.shutdown().await;
}

#[tokio::test]
async fn cancelled_termination_observer_preserves_service_and_single_result_consumption() {
    use std::future::Future;
    let (app, api) = setup().await;
    let mut server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Arc::new(api)).unwrap();
    let mut observer = Box::pin(server.terminated());
    assert!(
        observer
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(observer);
    assert!(server.task.is_some());
    let response = request(
        server.address(),
        &format!(
            "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"));
    server.stop.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), server.terminated())
        .await
        .unwrap()
        .unwrap();
    assert!(server.task.is_none());
    assert_eq!(server.slots.available_permits(), MAX_CONNECTIONS);
    // CLI takes this path when supervision wins its select before shutdown.
    server.shutdown().await.unwrap();
    app.shutdown().await;
}

#[tokio::test]
async fn cancelled_shutdown_still_drains_the_owned_network_tasks() {
    use std::future::Future;
    let (app, api) = setup().await;
    let server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Arc::new(api)).unwrap();
    let mut client = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    client.write_all(b"GET /").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.active_connections() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let slots = server.slots.clone();
    let mut shutdown = Box::pin(server.shutdown());
    assert!(
        shutdown
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(shutdown);
    let result = tokio::time::timeout(Duration::from_secs(3), client.read(&mut [0; 1]))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
    tokio::time::timeout(Duration::from_secs(3), async {
        while slots.available_permits() != MAX_CONNECTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    app.shutdown().await;
}

#[tokio::test]
async fn failed_listener_result_is_reported_once_without_repolling_completed_task() {
    let (stop, _receive) = oneshot::channel();
    // Inject the task's result, not an OS accept failure: this verifies the
    // supervision contract, not platform-specific socket failure handling.
    let mut server = AdminServer {
        address: "127.0.0.1:0".parse().unwrap(),
        stop: Some(stop),
        task: Some(tokio::spawn(async {
            Err(io::Error::other("injected listener failure"))
        })),
        slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
    };
    assert_eq!(
        server.terminated().await.unwrap_err().to_string(),
        "injected listener failure"
    );
    assert!(server.task.is_none());
    // The caller must preserve the error it received. Draining afterward does
    // not replay the receipt, nor poll the completed JoinHandle a second time.
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_parser_bounds_headers_and_times_out_partial_requests() {
    let (app, api) = setup().await;
    let server =
        AdminServer::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Arc::new(api)).unwrap();
    let many = request(
        server.address(),
        &format!(
            "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
            "X-Test: value\r\n".repeat(40)
        ),
    )
    .await;
    assert!(many.starts_with("HTTP/1.1 431"), "{many}");
    let large = request(
        server.address(),
        &format!(
            "GET /v1/config/version HTTP/1.1\r\nHost: localhost\r\nX-Test: {}\r\n\r\n",
            "a".repeat(20 * 1024)
        ),
    )
    .await;
    assert!(large.starts_with("HTTP/1.1 431"), "{large}");
    let mut partial = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    partial
        .write_all(b"GET /v1/config/version HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    tokio::time::pause();
    let mut output = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(6), partial.read_to_end(&mut output))
        .await
        .unwrap();
    assert!(
        read.is_ok()
            || read.as_ref().is_err_and(|error| matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ))
    );
    tokio::time::resume();
    // Dropping the owner also signals cancellation; explicit shutdown is still
    // required by production callers that need a completion receipt.
    let mut pending = tokio::net::TcpStream::connect(server.address()).await.unwrap();
    pending.write_all(b"GET /").await.unwrap();
    drop(server);
    let read = tokio::time::timeout(Duration::from_secs(2), pending.read(&mut [0; 1]))
        .await
        .unwrap();
    assert!(matches!(read, Ok(0) | Err(_)));
    app.shutdown().await;
}
