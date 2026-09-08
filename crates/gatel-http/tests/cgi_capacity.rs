//! Real preparation transactions must pass the same process pool to compatible
//! generations, not merely use equal numeric concurrency values.
mod support;
use std::sync::{Arc, Mutex};

use gatel_cgi_http::{Backend, BackendConfig, BackendFactory, ExchangeError, Invocation, Running, Sink};
use gatel_plan::{Bundle, DirectiveMeta, Outcome, Registry};
use tokio::sync::Semaphore;

#[derive(Debug)]
struct Factory(Arc<Mutex<Vec<Arc<Semaphore>>>>);
impl BackendFactory for Factory {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("capacity-cgi");
        &META
    }
    fn validate(&self, _: BackendConfig<'_>) -> Result<(), String> {
        Ok(())
    }
    fn prepare(&self, _: BackendConfig<'_>) -> Result<Arc<dyn Backend>, String> {
        panic!("host must supply capacity")
    }
    fn prepare_with_capacity(
        &self,
        config: BackendConfig<'_>,
        capacity: Arc<Semaphore>,
    ) -> Result<Arc<dyn Backend>, String> {
        self.0.lock().unwrap().push(capacity);
        if config.target == "reject" {
            return Err("test candidate rejected".into());
        }
        Ok(Arc::new(Unused))
    }
}
struct Unused;
impl Backend for Unused {
    fn run(&self, _: Invocation, _: Sink) -> Running {
        Box::pin(async { Err(ExchangeError::Failed) })
    }
}

#[tokio::test]
async fn process_pool_is_reused_for_compatible_reload_and_failed_candidate() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = support::Gateway::new("cgi-process-capacity");
    gateway.registry = Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .bundle(
            Bundle::new("capacity-probe", "1").with(gatel_cgi_http::backend_directive(Factory(seen.clone()))),
        )
        .build()
        .unwrap();
    std::fs::write(gateway.directory.join("app"), "fixture").unwrap();
    let port = support::free_port();
    let config = |target, count| {
        format!(
            r#"global {{ http "127.0.0.1:{port}"; }}
site "*" {{ route "/*" {{ capacity-cgi "{target}" root="." script="app" concurrency={count}; }} }}"#
        )
    };
    assert!(matches!(
        gateway.apply(&config("first", 1)).await,
        Outcome::Started { .. }
    ));
    let first = seen.lock().unwrap()[0].clone();
    let permit = first.clone().try_acquire_owned().unwrap();
    assert!(matches!(
        gateway.apply(&config("second", 1)).await,
        Outcome::Reloaded { .. }
    ));
    assert!(Arc::ptr_eq(&first, &seen.lock().unwrap()[1]));
    assert!(seen.lock().unwrap()[1].clone().try_acquire_owned().is_err());
    let rejected = gateway.apply(&config("reject", 1)).await;
    assert!(!matches!(
        rejected,
        Outcome::Started { .. } | Outcome::Reloaded { .. }
    ));
    assert!(Arc::ptr_eq(&first, &seen.lock().unwrap()[2]));
    assert!(!first.is_closed());
    drop(permit);
    assert_eq!(first.available_permits(), 1);
    assert!(matches!(
        gateway.apply(&config("third", 2)).await,
        Outcome::Reloaded { .. }
    ));
    assert!(!Arc::ptr_eq(&first, &seen.lock().unwrap()[3]));
    assert_eq!(seen.lock().unwrap()[3].available_permits(), 2);
    gateway.stop().await;
}
