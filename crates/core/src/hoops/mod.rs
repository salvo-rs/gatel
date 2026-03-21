pub mod acme_challenge;
pub mod auth;
pub mod buffer;
pub mod cache;
pub mod compress;
pub mod decompress;
pub mod error_pages;
pub mod forward_auth;
pub mod headers;
pub mod ip_filter;
pub mod logging;
pub mod metrics;
pub mod rate_limit;
pub mod replace;
pub mod rewrite;
pub mod stream_replace;
pub mod templates;

use std::net::SocketAddr;

/// Extract the client address from a Salvo request, falling back to 0.0.0.0:0.
pub fn client_addr(req: &salvo::Request) -> SocketAddr {
    req.remote_addr()
        .clone()
        .into_std()
        .unwrap_or_else(|| ([0, 0, 0, 0], 0).into())
}
