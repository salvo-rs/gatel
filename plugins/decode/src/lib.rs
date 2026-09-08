//! Bounded decode-before-forward: encoded input streams; validated output is
//! buffered so malformed or excessive bodies never partially reach a terminal.
#![forbid(unsafe_code)]
mod body;
#[cfg(test)]
mod tests;
use std::sync::Arc;

use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostics, FromNode as _, Hasher, Node};
use gatel_plugin_api::{DirectiveMeta, Effect, Phase, Scopes};
use gatel_plugin_runtime::{BuildContext, Bundle, Directive, DirectiveConfig, DirectiveState};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, FromNode)]
struct Config {
    #[gatel(prop, default = ByteSize(8 * 1024 * 1024))]
    max_encoded: ByteSize,
    #[gatel(prop, default = ByteSize(8 * 1024 * 1024))]
    max_decoded: ByteSize,
    #[gatel(prop, default = 100)]
    max_ratio: u32,
    #[gatel(prop, default = 8)]
    max_inflight: u32,
    #[gatel(prop, default = 10000)]
    timeout_ms: u64,
}
impl DirectiveConfig for Config {
    fn hash_into(&self, hash: &mut Hasher) {
        hash.update_u64(self.max_encoded.bytes());
        hash.update_u64(self.max_decoded.bytes());
        hash.update_u64(self.max_ratio as u64);
        hash.update_u64(self.max_inflight as u64);
        hash.update_u64(self.timeout_ms);
    }
    fn state(&self) -> Option<DirectiveState> {
        let mut compatibility = Hasher::new();
        compatibility.update_str("decode-admission-v1");
        compatibility.update_u64(self.max_inflight as u64);
        let capacity = self.max_inflight as usize;
        Some(DirectiveState {
            compatibility: compatibility.finish(),
            create: Box::new(move || Arc::new(Semaphore::new(capacity))),
        })
    }
    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Ok(Arc::new(DecodeHandler {
            config: self.clone(),
            budget: cx.state("decode admission")?,
        }))
    }
    fn describe(&self) -> String {
        format!(
            "decode encoded={} decoded={} ratio={} inflight={} timeout-ms={}",
            self.max_encoded, self.max_decoded, self.max_ratio, self.max_inflight, self.timeout_ms
        )
    }
}
pub struct Decode;
impl Directive for Decode {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("decode", Phase::Guard, Scopes::BOTH)
            .with_effects(&[Effect::TransformRequestBody])
            .conflicts_with(&["decode"]);
        &META
    }
    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let config = Config::from_node(node, diagnostics)?;
        if !(1..=64 * 1024 * 1024).contains(&config.max_encoded.bytes())
            || !(1..=64 * 1024 * 1024).contains(&config.max_decoded.bytes())
            || !(1..=1000).contains(&config.max_ratio)
            || !(1..=64).contains(&config.max_inflight)
            || !(1..=300_000).contains(&config.timeout_ms)
        {
            diagnostics.error(Code::InvalidValue, node.origin.clone(),
                "decode requires byte limits 1..=64MiB, ratio 1..=1000, inflight 1..=64 and timeout-ms 1..=300000");
            return None;
        }
        Some(Arc::new(config))
    }
}
pub fn bundle() -> Bundle {
    Bundle::new("decode", "1").with(Decode)
}
struct DecodeHandler {
    config: Config,
    budget: Arc<Semaphore>,
}
#[async_trait]
impl Handler for DecodeHandler {
    async fn handle(&self, req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
        let mut values = req.headers().get_all("content-encoding").iter();
        let Some(value) = values.next() else {
            return;
        };
        let encoding = value.to_str().unwrap_or("").trim().to_ascii_lowercase();
        if values.next().is_some()
            || !matches!(encoding.as_str(), "gzip" | "br" | "zstd" | "deflate" | "identity")
        {
            reject(depot, res, ctrl, StatusCode::UNSUPPORTED_MEDIA_TYPE);
            return;
        }
        if encoding == "identity" {
            return;
        }
        if req.headers().contains_key("content-range")
            || req.headers().get_all("cache-control").iter().any(|value| {
                value.to_str().is_ok_and(|value| {
                    value
                        .split(',')
                        .any(|part| part.trim().eq_ignore_ascii_case("no-transform"))
                })
            })
        {
            reject(depot, res, ctrl, StatusCode::UNSUPPORTED_MEDIA_TYPE);
            return;
        }
        let Ok(permit) = self.budget.clone().try_acquire_owned() else {
            reject(depot, res, ctrl, StatusCode::SERVICE_UNAVAILABLE);
            return;
        };
        let input = std::mem::take(req.body_mut());
        let mut config = self.config.clone();
        if let Some(limit) = gatel_plugin_runtime::RequestBodyLimit::get(depot) {
            config.max_decoded = ByteSize(config.max_decoded.bytes().min(limit));
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(self.config.timeout_ms),
            body::decode(input, &encoding, &config),
        )
        .await;
        let data = match result {
            Ok(Ok(data)) => data,
            Ok(Err(status)) => {
                reject(depot, res, ctrl, status);
                return;
            }
            Err(_) => {
                reject(depot, res, ctrl, StatusCode::REQUEST_TIMEOUT);
                return;
            }
        };
        // The permit follows every Bytes clone, including a body moved into an
        // upstream request or retained by a downstream handler.
        let bytes = bytes::Bytes::from_owner(body::Decoded {
            data,
            _permit: permit,
        });
        for name in [
            "content-encoding",
            "content-length",
            "transfer-encoding",
            "trailer",
            "content-md5",
            "digest",
            "content-digest",
            "repr-digest",
        ] {
            req.headers_mut().remove(name);
        }
        req.headers_mut().insert("content-length", bytes.len().into());
        *req.body_mut() = salvo_core::http::ReqBody::Once(bytes);
        ctrl.call_next(req, depot, res).await;
    }
}
fn reject(depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl, status: StatusCode) {
    depot.insert("gatel.generated", true);
    res.status_code(status);
    let _ = res.write_body("request decoding rejected\n");
    ctrl.skip_rest();
}
