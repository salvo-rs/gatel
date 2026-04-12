pub mod admin;
mod cache_control;
pub mod config;
mod crypto;
mod encoding;
pub mod events;
mod glob;
pub mod goals;
pub mod hoops;
pub mod observability;
mod placeholder;
pub mod plugin;
pub mod proxy;
pub mod router;
pub mod runtime;
pub mod runtime_services;
pub mod salvo_service;
pub mod sd_notify;
pub mod server;
pub mod storage;
pub mod stream;
pub mod tls;
pub mod ttl_cache;
mod websocket;

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;

/// The body type used throughout gatel — error is a boxed trait object
/// so we can unify bodies from different sources (hyper Incoming, Full, Empty, upstream).
pub type Body = BoxBody<Bytes, BoxError>;

/// Boxed error type alias.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Create an empty body.
pub fn empty_body() -> Body {
    use http_body_util::Empty;
    BoxBody::new(
        Empty::new().map_err(|never: std::convert::Infallible| -> BoxError { match never {} }),
    )
}

/// Create a body from bytes.
pub fn full_body(data: impl Into<Bytes>) -> Body {
    use http_body_util::Full;
    BoxBody::new(
        Full::new(data.into())
            .map_err(|never: std::convert::Infallible| -> BoxError { match never {} }),
    )
}

/// Top-level error type for the proxy.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    #[error("hyper-util client error: {0}")]
    Client(#[from] hyper_util::client::legacy::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(#[from] config::ConfigError),

    #[error("no upstream available")]
    NoUpstream,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("{0}")]
    Internal(String),
}

impl ProxyError {
    /// Convert this error into an HTTP response.
    pub fn into_response(self) -> http::Response<Body> {
        let status = match &self {
            ProxyError::BadRequest(_) => http::StatusCode::BAD_REQUEST,
            ProxyError::NoUpstream => http::StatusCode::BAD_GATEWAY,
            _ => http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = full_body(self.to_string());
        http::Response::builder().status(status).body(body).unwrap()
    }
}
