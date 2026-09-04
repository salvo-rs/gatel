//! What a reload does, and what it refuses to do.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use gatel_plan::Outcome;
use support::*;

#[tokio::test]
async fn a_reload_changes_behaviour_without_rebinding_the_socket() {
    let gateway = Gateway::new("reload");
    let port = free_port();
    let config = |body: &str| {
        format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" {{ route "/" {{ respond "{body}" }} }}
"#
        )
    };

    gateway.apply(&config("before")).await;
    let address = gateway.address();
    let socket = gateway.binds.get(&address).expect("bound");
    assert!(get(address, "/").await.contains("before"));

    let outcome = gateway.apply(&config("after")).await;
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    assert!(get(address, "/").await.contains("after"));
    assert!(
        Arc::ptr_eq(&socket, &gateway.binds.get(&address).unwrap()),
        "a reload must not close and reopen the listening socket"
    );
}

#[tokio::test]
async fn a_cosmetic_edit_does_not_produce_a_generation() {
    let gateway = Gateway::new("noop");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"same\" }} }}\n"
        ))
        .await;

    let outcome = gateway
        .apply(&format!(
            "// a comment\nglobal {{\n    http \"127.0.0.1:{port}\"\n}}\n\nsite \"*\" {{\n    route \"/\" {{ respond \"same\" }}\n}}\n"
        ))
        .await;
    assert!(matches!(outcome, Outcome::NoOp { .. }), "{outcome:?}");
    assert_eq!(gateway.store.status().generation.number(), 1);
}

#[tokio::test]
async fn a_rejected_reload_keeps_serving_the_last_good_configuration() {
    let gateway = Gateway::new("lkg");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"good\" }} }}\n"
        ))
        .await;
    let address = gateway.address();

    let outcome = gateway.apply("global { nonsense }\n").await;
    assert!(matches!(outcome, Outcome::Rejected(_)), "{outcome:?}");
    assert!(
        get(address, "/").await.contains("good"),
        "the previous generation must still serve"
    );

    let status = gateway.store.status();
    assert!(status.is_serving());
    assert!(
        status.is_degraded(),
        "the operator must be able to see that the newest attempt failed"
    );

    // And recovery clears it.
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"recovered\" }} }}\n"
        ))
        .await;
    assert!(get(address, "/").await.contains("recovered"));
    assert!(!gateway.store.status().is_degraded());
}

#[tokio::test]
async fn a_port_conflict_fails_the_reload_and_changes_nothing() {
    let gateway = Gateway::new("conflict");
    let port = free_port();
    gateway
        .apply(&format!(
            "global {{ http \"127.0.0.1:{port}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"first\" }} }}\n"
        ))
        .await;
    let address = gateway.address();

    // Take a port, then ask for it.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = taken.local_addr().unwrap().port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{
    http "a" "127.0.0.1:{port}"
    http "b" "127.0.0.1:{occupied}"
}}
site "*" {{ route "/" {{ respond "second" }} }}
"#
        ))
        .await;
    assert!(matches!(outcome, Outcome::PrepareFailed(_)), "{outcome:?}");
    assert_eq!(gateway.store.status().generation.number(), 1);
    assert!(get(address, "/").await.contains("first"));
}

#[tokio::test]
async fn adding_and_removing_a_listener_works_without_a_restart() {
    let gateway = Gateway::new("listeners");
    let first = free_port();
    let second = free_port();

    gateway
        .apply(&format!(
            "global {{ http \"a\" \"127.0.0.1:{first}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"one\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 1);

    gateway
        .apply(&format!(
            r#"
global {{
    http "a" "127.0.0.1:{first}"
    http "b" "127.0.0.1:{second}"
}}
site "*" {{ route "/" {{ respond "two" }} }}
"#
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 2);
    let second_address: SocketAddr = format!("127.0.0.1:{second}").parse().unwrap();
    assert!(get(second_address, "/").await.contains("two"));

    gateway
        .apply(&format!(
            "global {{ http \"a\" \"127.0.0.1:{first}\" }}\nsite \"*\" {{ route \"/\" {{ respond \"one again\" }} }}\n"
        ))
        .await;
    assert_eq!(gateway.server.active_addresses().await.len(), 1);
    assert!(
        gateway.binds.get(&second_address).is_none(),
        "the removed socket must be closed"
    );
}
