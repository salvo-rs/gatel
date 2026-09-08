//! Complete source unions use the real compiler, publication and HTTP runtime.
mod support;
use std::sync::Arc;
use std::time::Duration;

use gatel_model::{Hasher, Snapshot, SourceId, SourceRev};
use gatel_plan::Outcome;
use support::*;

fn observe(inputs: &mut Snapshot, source: SourceId, text: &str) {
    let expected = inputs.get(&source).map(|source| source.rev);
    let mut hash = Hasher::new();
    hash.update_str(text);
    let nodes = gatel_kdl::lower(text, &source.to_string()).unwrap();
    inputs.observe_if(source, expected, hash.finish(), nodes).unwrap();
}

#[tokio::test]
async fn deleting_a_cross_source_dependency_preserves_lkg_until_reference_is_removed() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let gateway = Gateway::new("source-composition");
        let address = gateway
            .binds
            .adopt(std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .unwrap();
        let upstream = backend("shared-backend").await;
        let file = SourceId::file("main");
        let admin = SourceId::admin();
        let file_config = |terminal: &str| {
            format!("global {{ http \"{address}\" }}\nsite \"*\" {{ route \"/\" {{ {terminal}; }} }}")
        };
        let mut inputs = Snapshot::new();
        observe(
            &mut inputs,
            file.clone(),
            &file_config("proxy upstream=\"runtime/shared\""),
        );
        observe(
            &mut inputs,
            admin.clone(),
            &format!("upstream \"runtime/shared\" {{ endpoint \"{upstream}\"; }}"),
        );
        let started = gateway.apply_snapshot(&inputs).await;
        assert!(matches!(started, Outcome::Started { .. }), "{started:?}");
        let response = get(address, "/").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("shared-backend"), "{response}");
        let published = gateway.store.current().unwrap();

        observe(&mut inputs, admin.clone(), "");
        let outcome = gateway.apply_snapshot(&inputs).await;
        let Outcome::Rejected(diagnostics) = outcome else {
            panic!("{outcome:?}")
        };
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == gatel_model::Code::UnresolvedReference
                    && d.message.contains("runtime/shared")),
            "{diagnostics}"
        );
        assert!(Arc::ptr_eq(&published, &gateway.store.current().unwrap()));
        assert_eq!(gateway.store.status().revs.get(&admin), Some(SourceRev(2)));
        assert_eq!(published.revs.get(&admin), Some(SourceRev(1)));
        assert!(get(address, "/").await.contains("shared-backend"));

        // Repair the file-owned reference, keeping the latest admin tombstone.
        observe(&mut inputs, file.clone(), &file_config("respond \"independent\""));
        assert!(matches!(
            gateway.apply_snapshot(&inputs).await,
            Outcome::Reloaded { .. }
        ));
        assert!(get(address, "/").await.ends_with("independent"));
        let current = gateway.store.current().unwrap();
        assert_eq!(current.revs.get(&file), Some(SourceRev(2)));
        assert_eq!(current.revs.get(&admin), Some(SourceRev(2)));
        assert!(current.plan.spec.upstreams.is_empty());
        drop(current);
        drop(published);
        gateway.stop().await;
    })
    .await
    .expect("multi-source HTTP transaction timed out");
}
