//! Host shutdown closes log queues even while a serving generation is retained.
use super::*;

#[tokio::test]
async fn shutdown_flushes_and_closes_writers_held_by_retained_generations() {
    let directory = std::env::temp_dir().join(format!("gatel-log-shutdown-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let app = Arc::new(App::new(directory.join("gatel.kdl")).unwrap());
    let address = app.binds.adopt(listener).unwrap();
    std::fs::write(&app.root, format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ access-log output=\"file:access.log\"; respond \"ok\"; }} }}"
    )).unwrap();
    let result = app.attempt();
    assert!(!result.is_failure(), "{result:?}");
    app.activate().await;
    let committed = app.store.current().unwrap();
    let sinks = app.telemetry.sinks.snapshot();
    assert_eq!(sinks.len(), 1);
    let path = directory.join("access.log");
    sinks[0].write("before shutdown");
    app.shutdown().await;
    assert_eq!(app.telemetry.sinks.pending_writers(), 0);
    assert_eq!(app.telemetry.sinks.shutdown_timeouts(), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before shutdown\n");
    sinks[0].write("after shutdown");
    assert_eq!(sinks[0].dropped(), 1);
    assert!(Arc::ptr_eq(&committed, &app.store.current().unwrap()));
    // These generation/sink handles remain alive after worker completion.
    std::fs::remove_file(path).unwrap();
    drop((sinks, committed, app));
    std::fs::remove_file(directory.join("gatel.kdl")).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
