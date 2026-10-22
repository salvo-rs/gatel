//! Optional control-plane HTTP handler. The loopback listener owns connection
//! limits and shutdown; explicit CLI opt-in never enables remote plaintext.
use std::sync::{Arc, Weak};

use bytes::Bytes;
use gatel_model::{Fingerprint, Hasher, SourceId};
use gatel_plan::{Outcome, SourceWriteError};
use http::{Request, Response, StatusCode, header};
use http_body::Body;
use http_body_util::{BodyExt, Full};
use subtle::ConstantTimeEq;

use crate::app::{App, AppSourceWriter};

#[path = "admin_cli.rs"]
pub(crate) mod cli;
#[path = "admin_journal.rs"]
mod journal;
#[allow(unused_imports, reason = "Host checkpoint integration boundary")]
pub(crate) use journal::checkpoint_writer::CheckpointWriter;
pub(crate) use journal::{bootstrap, cli as journal_cli};
#[path = "admin_server.rs"]
pub(crate) mod server;

#[path = "admin_status.rs"]
mod status;

const MAX_BODY: usize = 1024 * 1024;
const MAX_FRAMES: usize = 65_536;

pub(crate) struct AdminApi {
    app: Weak<App>,
    writer: AppSourceWriter,
    token: Fingerprint,
    read_token: Option<Fingerprint>,
    requests: tokio::sync::Semaphore,
    // Small independent reserve: slow writes must not consume diagnostic reads.
    read_requests: tokio::sync::Semaphore,
    checkpoint_mode: bool,
}

fn digest(token: &str) -> Fingerprint {
    let mut hash = Hasher::new();
    hash.update_str("gatel.admin-bearer.v1");
    hash.update_str(token);
    hash.finish()
}

fn validate_token(token: &str) -> Result<(), &'static str> {
    if !(32..=256).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("admin token must contain 32 to 256 non-whitespace ASCII bytes");
    }
    Ok(())
}

impl AdminApi {
    /// Trusted host opt-in. All other participating sources must use the same
    /// checkpoint policy; this constructor alone does not enable durable run.
    pub(crate) fn with_checkpoint(
        app: &Arc<App>,
        token: zeroize::Zeroizing<String>,
        checkpoint: Arc<std::sync::Mutex<CheckpointWriter>>,
    ) -> Result<Self, &'static str> {
        let mut api = Self::new(app, token)?;
        api.writer = app
            .checkpoint_source_writer(SourceId::admin(), checkpoint)
            .ok_or("admin source is not available")?;
        api.checkpoint_mode = true;
        Ok(api)
    }

    /// Token must be supplied privately by the host, not a command-line value.
    /// Length/syntax checks do not prove entropy; provision a random secret.
    pub fn new(app: &Arc<App>, token: zeroize::Zeroizing<String>) -> Result<Self, &'static str> {
        validate_token(&token)?;
        let writer = app
            .source_writer(SourceId::admin())
            .ok_or("admin source is not available")?;
        Ok(Self {
            app: Arc::downgrade(app),
            writer,
            token: digest(&token),
            read_token: None,
            requests: tokio::sync::Semaphore::new(8),
            read_requests: tokio::sync::Semaphore::new(2),
            checkpoint_mode: false,
        })
    }

    /// Trusted host provisioning only. A read token cannot issue source writes.
    pub(crate) fn with_read_token(
        mut self,
        token: Option<zeroize::Zeroizing<String>>,
    ) -> Result<Self, &'static str> {
        if let Some(token) = token {
            validate_token(&token)?;
            let hashed = digest(&token);
            if bool::from(hashed.as_bytes().ct_eq(self.token.as_bytes())) {
                return Err("admin read and write tokens must differ");
            }
            self.read_token = Some(hashed);
        }
        Ok(self)
    }

    pub async fn handle<B>(&self, request: Request<B>) -> Response<Full<Bytes>>
    where
        B: Body<Data = Bytes> + Unpin,
    {
        let headers = request.headers();
        let authorization = single(headers, header::AUTHORIZATION).and_then(|s| s.strip_prefix("Bearer "));
        let (can_write, can_read) = authorization
            .filter(|s| s.len() <= 256)
            .map(|s| {
                let hashed = digest(s);
                let write = bool::from(hashed.as_bytes().ct_eq(self.token.as_bytes()));
                let read = self
                    .read_token
                    .as_ref()
                    .is_some_and(|token| bool::from(hashed.as_bytes().ct_eq(token.as_bytes())));
                (write, read)
            })
            .unwrap_or_default();
        if !can_write && !can_read {
            let mut result = reply(StatusCode::UNAUTHORIZED, "unauthorized");
            result.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                http::HeaderValue::from_static("Bearer realm=\"gatel-admin\""),
            );
            return result;
        }
        // No browser cross-origin management, including requests from opaque origins.
        if headers.contains_key(header::ORIGIN) {
            return reply(StatusCode::FORBIDDEN, "origin not allowed");
        }
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("");
        let read_only = request.method() == http::Method::GET
            && matches!(path, "/v1/config/version" | "/v1/config/status");
        if !can_write && !read_only {
            return reply(StatusCode::FORBIDDEN, "read-only credential");
        }
        let capacity = if read_only {
            &self.read_requests
        } else {
            &self.requests
        };
        let Ok(_permit) = capacity.try_acquire() else {
            return reply(StatusCode::SERVICE_UNAVAILABLE, "admin capacity exhausted");
        };
        let Some(app) = self.app.upgrade().filter(|app| !app.configuration_closed()) else {
            return reply(StatusCode::SERVICE_UNAVAILABLE, "admin unavailable");
        };
        if path == "/v1/config/status" {
            if request.method() != http::Method::GET {
                let mut response = reply(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
                response
                    .headers_mut()
                    .insert(header::ALLOW, http::HeaderValue::from_static("GET"));
                return response;
            }
            let (status, activated) = app.configuration_progress();
            return status::reply(
                &status,
                activated,
                &app.telemetry.health.report(),
                self.checkpoint_mode,
            );
        }
        let status = app.configuration_status();
        let version = status.version();
        if path == "/v1/config/version" && request.method() == http::Method::GET {
            let mut response = reply(StatusCode::OK, "configuration version");
            response
                .headers_mut()
                .insert(header::ETAG, version.etag().parse().unwrap());
            // Same store sample as the ETag; never infer rejection or durability
            // from provenance differences (NoOp also retains old build input).
            response.headers_mut().insert(
                "x-gatel-observed-differs",
                http::HeaderValue::from_static(if status.observed_differs { "true" } else { "false" }),
            );
            return response;
        }
        if path != "/v1/config" {
            return reply(StatusCode::NOT_FOUND, "not found");
        }
        if request.method() != http::Method::PUT {
            let mut response = reply(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, http::HeaderValue::from_static("PUT"));
            return response;
        }
        if !headers.contains_key(header::IF_MATCH) {
            return reply(StatusCode::PRECONDITION_REQUIRED, "If-Match required");
        }
        if single(headers, header::IF_MATCH) != Some(version.etag().as_str()) {
            return reply(StatusCode::PRECONDITION_FAILED, "configuration changed");
        }
        if single(headers, header::CONTENT_TYPE) != Some("application/kdl")
            || headers.contains_key(header::CONTENT_ENCODING)
        {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected unencoded application/kdl",
            );
        }
        if headers.contains_key(header::CONTENT_LENGTH) && headers.contains_key(header::TRANSFER_ENCODING) {
            return reply(StatusCode::BAD_REQUEST, "ambiguous body framing");
        }
        let length = if headers.contains_key(header::CONTENT_LENGTH) {
            match single(headers, header::CONTENT_LENGTH)
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u64>().ok())
            {
                Some(n) if n <= MAX_BODY as u64 => Some(n as usize),
                Some(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
                None => return reply(StatusCode::BAD_REQUEST, "invalid content length"),
            }
        } else {
            None
        };
        let body =
            match tokio::time::timeout(std::time::Duration::from_secs(5), read_body(request.into_body()))
                .await
            {
                Ok(Ok(body)) => body,
                Ok(Err(status)) => return reply(status, "invalid request body"),
                Err(_) => return reply(StatusCode::REQUEST_TIMEOUT, "request body timeout"),
            };
        if length.is_some_and(|n| n != body.len()) {
            return reply(StatusCode::BAD_REQUEST, "content length mismatch");
        }
        let Ok(text) = String::from_utf8(body) else {
            return reply(StatusCode::BAD_REQUEST, "expected UTF-8");
        };
        // Do not attach a freshly sampled ETag to this outcome: another write may
        // already have happened. Clients fetch a new validator before retrying.
        write_reply(
            self.writer.reload_kdl_if(version, text).await,
            self.checkpoint_mode,
        )
    }
}

fn write_reply(result: Result<Outcome, SourceWriteError>, checkpoint: bool) -> Response<Full<Bytes>> {
    let durability = match &result {
        Ok(Outcome::Started { .. } | Outcome::Reloaded { .. } | Outcome::NoOp { .. }) => {
            Some(if checkpoint { "checkpoint" } else { "memory" })
        }
        Err(SourceWriteError::PersistenceUnknown) => Some("unknown"),
        _ => None,
    };
    let mut response = match result {
        Ok(Outcome::Started { .. } | Outcome::Reloaded { .. } | Outcome::NoOp { .. }) => reply(
            StatusCode::OK,
            if checkpoint {
                "applied and checkpoint saved"
            } else {
                "applied in memory"
            },
        ),
        Ok(Outcome::Rejected(_)) | Err(SourceWriteError::Observation(_)) => {
            reply(StatusCode::UNPROCESSABLE_ENTITY, "configuration rejected")
        }
        Ok(Outcome::PrepareFailed(_)) => reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "configuration preparation failed",
        ),
        Err(SourceWriteError::Conflict(_)) => reply(StatusCode::PRECONDITION_FAILED, "configuration changed"),
        Ok(Outcome::Stale(_)) | Err(SourceWriteError::Unavailable) => {
            reply(StatusCode::SERVICE_UNAVAILABLE, "configuration unavailable")
        }
        Err(SourceWriteError::PersistenceUnknown) => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration may be applied; persistence outcome unknown; inspect before retrying",
        ),
    };
    // A write result, not a durable request receipt or a status-query promise.
    if let Some(durability) = durability {
        response
            .headers_mut()
            .insert("x-gatel-durability", http::HeaderValue::from_static(durability));
    }
    response
}

fn single(headers: &http::HeaderMap, key: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(key).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}

async fn read_body<B: Body<Data = Bytes> + Unpin>(mut body: B) -> Result<Vec<u8>, StatusCode> {
    let mut bytes = Vec::new();
    let mut frames = 0;
    while let Some(frame) = body.frame().await {
        frames += 1;
        if frames > MAX_FRAMES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        let data = frame.into_data().map_err(|_| StatusCode::BAD_REQUEST)?;
        if data.len() > MAX_BODY - bytes.len() {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        bytes.extend_from_slice(&data);
    }
    Ok(bytes)
}

fn reply(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        // A future HTTP/2 adapter must remove connection-specific headers.
        .header(header::CONNECTION, "close")
        .body(Full::new(Bytes::from_static(message.as_bytes())))
        .unwrap()
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "admin_credentials_tests.rs"]
mod credentials_tests;
