//! Optional source-linked plugin; no sockets, mutable state or background tasks.
#![forbid(unsafe_code)]
use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plugin_api::{DirectiveMeta, Phase, Scopes};
use gatel_plugin_runtime::{BuildContext, Bundle, Directive, DirectiveConfig};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, Clone, FromNode)]
struct Config {
    #[gatel(prop, default = true)]
    enabled: bool,
    #[gatel(prop, default = 60)]
    retry_after: u32,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hash: &mut Hasher) {
        hash.update_bool(self.enabled);
        hash.update_u64(self.retry_after as u64);
    }
    fn build(&self, _: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(self.clone()))
    }
    fn describe(&self) -> String {
        format!(
            "maintenance enabled={} retry-after={}",
            self.enabled, self.retry_after
        )
    }
}

#[async_trait]
impl Handler for Config {
    async fn handle(&self, req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
        if !self.enabled {
            return;
        }
        depot.insert("gatel.generated", true);
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        res.headers_mut()
            .insert("retry-after", self.retry_after.to_string().parse().unwrap());
        res.headers_mut()
            .insert("cache-control", "no-store".parse().unwrap());
        res.headers_mut()
            .insert("content-type", "text/plain; charset=utf-8".parse().unwrap());
        let body = "Service temporarily unavailable\n";
        res.headers_mut()
            .insert("content-length", body.len().to_string().parse().unwrap());
        if req.method().as_str() != "HEAD" {
            let _ = res.write_body(body);
        }
        ctrl.skip_rest();
    }
}

pub struct Maintenance;
impl Directive for Maintenance {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("maintenance", Phase::Guard, Scopes::BOTH)
            .before(&["rate-limit", "forward-auth"]);
        &META
    }
    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let config = Config::from_node(node, diagnostics)?;
        if !(1..=86400).contains(&config.retry_after) {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "retry-after must be 1..=86400 seconds",
            );
            return None;
        }
        Some(Arc::new(config))
    }
}
pub fn bundle() -> Bundle {
    Bundle::new("maintenance", "1").with(Maintenance)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn enabled_refuses_and_disabled_leaves_the_response_alone() {
        for enabled in [false, true] {
            let mut req = Request::default();
            let mut res = Response::default();
            let mut depot = Depot::new();
            let mut ctrl = FlowCtrl::new(vec![]);
            Config {
                enabled,
                retry_after: 42,
            }
            .handle(&mut req, &mut depot, &mut res, &mut ctrl)
            .await;
            if enabled {
                assert_eq!(res.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
                assert_eq!(res.headers()["retry-after"], "42");
                assert_eq!(res.headers()["cache-control"], "no-store");
            } else {
                assert_eq!(res.status_code, None);
            }
        }
    }
}
