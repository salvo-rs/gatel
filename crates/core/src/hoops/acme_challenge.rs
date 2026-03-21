//! ACME HTTP-01 challenge middleware.
//!
//! Intercepts requests to `/.well-known/acme-challenge/<token>` and responds
//! with the corresponding key authorization from certon's
//! [`HttpChallengeHandler`]. Non-challenge requests pass through to the next
//! middleware in the chain.

use std::collections::HashMap;
use std::sync::Arc;

use certon::Storage;
use certon::http_handler::HttpChallengeHandler;
use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Middleware that intercepts ACME HTTP-01 challenge validation requests.
///
/// When the CA sends a `GET /.well-known/acme-challenge/<token>` request to
/// validate domain ownership, this middleware responds with the key
/// authorization string. All other requests are passed through unchanged.
///
/// The middleware wraps certon's [`HttpChallengeHandler`], which checks
/// both a shared in-memory challenge map and (optionally) distributed
/// storage for the token.
pub struct AcmeChallengeHoop {
    handler: HttpChallengeHandler,
}

impl AcmeChallengeHoop {
    /// Create a new ACME challenge middleware.
    ///
    /// `challenges` is the shared in-memory map of token -> key_auth,
    /// populated by the HTTP-01 solver when certon presents a challenge.
    /// This should be the same `Arc` that the [`TlsManager`](crate::tls::TlsManager)
    /// exposes via its `challenge_map()` method.
    ///
    /// `storage` is an optional shared storage backend for distributed
    /// challenge solving across multiple proxy instances.
    pub fn new(
        challenges: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
        storage: Option<Arc<dyn Storage>>,
    ) -> Self {
        Self {
            handler: HttpChallengeHandler::new(challenges, storage),
        }
    }
}

#[async_trait]
impl salvo::Handler for AcmeChallengeHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let method = req.method().as_str().to_owned();
        let uri = req.uri().clone();
        let path = uri.path();
        let host = req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        // Use the full-featured handler with method + host validation.
        if let Some((status, body)) = self.handler.handle_http_request(&method, host, path).await {
            let client_addr = super::client_addr(req);
            debug!(
                path = %path,
                status = status,
                client = %client_addr,
                "served ACME HTTP-01 challenge response"
            );

            let http_status =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            res.status_code(http_status);
            let _ = res.add_header(http::header::CONTENT_TYPE, "text/plain", true);
            res.body(body);
            ctrl.skip_rest();
            return;
        }

        // Not a challenge request -- pass through.
        ctrl.call_next(req, depot, res).await;
    }
}
