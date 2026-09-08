//! The development file server is a typed configuration frontend, not a second server.
use std::net::SocketAddr;
use std::path::PathBuf;

use gatel_model::{Hasher, Node, Origin, Snapshot, SourceId, SourceRev, SourceSnapshot, Value};

use crate::app::App;

pub(crate) fn app(root: PathBuf, address: SocketAddr, browse: bool) -> Result<App, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot open static root: {error}"))?;
    if !root.is_dir() {
        return Err("static root must be a directory".into());
    }
    // Keep the filesystem path as PathBuf. The file-server receives '.' relative
    // to Resources.root; quotes, newlines and non-Unicode paths are not KDL text.
    App::from_snapshot(root, snapshot(address, browse))
}

fn snapshot(address: SocketAddr, browse: bool) -> Snapshot {
    let origin = Origin::synthetic("serve command");
    let text = |value: &str| Value::string(value, origin.clone());
    let nodes = vec![
        Node::new("global", origin.clone())
            .with_child(Node::new("http", origin.clone()).with_arg(text(&address.to_string()))),
        Node::new("site", origin.clone()).with_arg(text("*")).with_child(
            Node::new("route", origin.clone())
                .with_arg(text("/*"))
                .with_child(
                    Node::new("file-server", origin.clone())
                        .with_prop("root", text("."))
                        .with_prop("browse", Value::bool(browse, origin.clone())),
                ),
        ),
    ];
    let mut hash = Hasher::new();
    hash.update_str("gatel-serve-input-v1");
    hash.update_str(&address.to_string());
    hash.update_bool(browse);
    Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("serve"),
        SourceRev(1),
        hash.finish(),
        nodes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn immutable_frontend_uses_normal_transactions_without_reading_config() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let app =
            std::sync::Arc::new(app(PathBuf::from(env!("CARGO_MANIFEST_DIR")), address, false).unwrap());
        assert_eq!(app.binds.adopt(socket).unwrap(), address);
        assert!(!app.watches_configuration());
        assert!(app.watched_paths().is_empty());
        app.compile().unwrap();
        assert!(matches!(app.reload().await, gatel_plan::Outcome::Started { .. }));
        assert!(matches!(app.reload().await, gatel_plan::Outcome::NoOp { .. }));
        app.shutdown().await;
    }
}
