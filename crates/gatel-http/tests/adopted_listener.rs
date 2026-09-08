//! Embedded sockets participate in the normal configuration transaction.
mod support;
use std::sync::Arc;
use std::time::Duration;

use gatel_plan::Outcome;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn queued_http_survives_failed_prepare_and_adopted_socket_survives_reload() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let gateway = Gateway::new("adopted-listener");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let mut queued = tokio::net::TcpStream::connect(address).await.unwrap();
        queued.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n").await.unwrap();
        assert_eq!(gateway.binds.adopt(socket).unwrap(), address);
        let listener = gateway.binds.get(&address).unwrap();
        let weak = Arc::downgrade(&listener);
        let config = |body: &str| format!("global {{ http \"{address}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"{body}\"; }} }}");

        // Failure after binding must not discard the host's existing socket or
        // publish the candidate, even for the very first configuration.
        let invalid = format!("global {{ http \"{address}\" }}\nsite \"*\" {{ route \"/\" {{ file-server root=\"missing-root\"; }} }}");
        let outcome = gateway.apply(&invalid).await;
        assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
        assert!(gateway.store.current().is_none());
        assert!(gateway.server.active_addresses().await.is_empty());
        assert!(Arc::ptr_eq(&listener, &gateway.binds.get(&address).unwrap()));

        assert!(matches!(gateway.apply(&config("first")).await, Outcome::Started { .. }));
        let mut response = Vec::new();
        queued.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("first"), "{response}");
        drop(queued);

        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let blocked = occupied.local_addr().unwrap();
        let conflict = format!("global {{ http \"original\" \"{address}\"; http \"conflict\" \"{blocked}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"unpublished\"; }} }}");
        assert!(matches!(gateway.apply(&conflict).await, Outcome::PrepareFailed(_)));
        assert!(get(address, "/").await.ends_with("first"));
        assert!(matches!(gateway.apply(&config("second")).await, Outcome::Reloaded { .. }));
        assert!(get(address, "/").await.ends_with("second"));
        assert!(Arc::ptr_eq(&listener, &gateway.binds.get(&address).unwrap()));

        drop(listener);
        gateway.stop().await;
        assert!(gateway.binds.bound().is_empty());
        assert!(gateway.server.active_addresses().await.is_empty());
        assert!(weak.upgrade().is_none(), "retired adopted socket is still owned");
    }).await.expect("adopted HTTP listener lifecycle timed out");
}
