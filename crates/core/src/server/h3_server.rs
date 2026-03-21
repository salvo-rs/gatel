//! HTTP/3 (QUIC) server implementation.
//!
//! This module is only compiled when the `http3` feature flag is enabled.
//! It runs an HTTP/3 listener alongside the existing HTTP/1+2 listeners,
//! sharing the same TLS configuration and Salvo routing infrastructure.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use h3::server::RequestResolver;
use http::{Request, Response};
use http_body_util::BodyExt;
use tracing::{debug, info};

use super::AppState;
use crate::{Body, ProxyError};

/// Run the HTTP/3 (QUIC) server on the given address.
///
/// This creates a QUIC endpoint using the provided `rustls::ServerConfig`,
/// accepts incoming QUIC connections, and serves HTTP/3 requests through
/// the same Salvo Service as the HTTP/1+2 server.
pub async fn run_h3_server(
    addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
    state: Arc<AppState>,
) -> Result<(), ProxyError> {
    // Build a QUIC-compatible server config from the existing rustls config.
    //
    // Quinn requires ALPN protocols to include "h3" for HTTP/3.
    // We clone the rustls config and set the ALPN before converting.
    let mut rustls_config = (*tls_config).clone();
    rustls_config.alpn_protocols = vec![b"h3".to_vec()];

    let quic_server_config = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .map_err(|e| ProxyError::Internal(format!("failed to create QUIC server config: {e}")))?;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server_config));

    let endpoint = quinn::Endpoint::server(server_config, addr).map_err(|e| {
        ProxyError::Internal(format!("failed to bind QUIC endpoint on {addr}: {e}"))
    })?;

    info!(%addr, "listening for HTTP/3 (QUIC) connections");

    loop {
        if state.shutdown.is_shutdown() {
            info!("HTTP/3 accept loop stopping (shutdown)");
            break;
        }

        let Some(incoming) = endpoint.accept().await else {
            info!("QUIC endpoint closed");
            break;
        };

        let state = Arc::clone(&state);
        let _conn_guard = state.shutdown.track_conn();

        tokio::spawn(async move {
            let _guard = _conn_guard;

            let client_addr = incoming.remote_address();
            if let Err(e) = handle_h3_connection(incoming, client_addr, state).await {
                debug!(client = %client_addr, "HTTP/3 connection error: {e}");
            }
        });
    }

    // Gracefully close the endpoint: reject new connections and wait for
    // existing ones to finish.
    endpoint.close(quinn::VarInt::from_u32(0), b"server shutting down");

    Ok(())
}

/// Handle a single QUIC connection, serving HTTP/3 requests over it.
async fn handle_h3_connection(
    incoming: quinn::Incoming,
    client_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let quinn_conn = incoming.await?;
    debug!(client = %client_addr, "QUIC connection established");

    let h3_conn = h3_quinn::Connection::new(quinn_conn);
    let mut server_conn: h3::server::Connection<h3_quinn::Connection, Bytes> =
        h3::server::Connection::new(h3_conn).await?;

    loop {
        let resolver: Option<RequestResolver<h3_quinn::Connection, Bytes>> =
            match server_conn.accept().await {
                Ok(resolver) => resolver,
                Err(e) => {
                    debug!(client = %client_addr, "H3 accept error: {e}");
                    return Err(Box::new(e));
                }
            };

        let Some(resolver) = resolver else {
            // Connection is closing (received GOAWAY or stream ended).
            debug!(client = %client_addr, "H3 connection closing gracefully");
            break;
        };

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_h3_request(resolver, client_addr, state).await {
                debug!(client = %client_addr, "H3 request error: {e}");
            }
        });
    }

    Ok(())
}

/// Handle a single HTTP/3 request: read headers + body, route through the
/// Salvo service, and write the response back over the QUIC stream.
async fn handle_h3_request(
    resolver: RequestResolver<h3_quinn::Connection, Bytes>,
    client_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Resolve the request headers and obtain the bidirectional stream.
    let (req, mut stream) = resolver.resolve_request().await?;

    debug!(
        client = %client_addr,
        method = %req.method(),
        uri = %req.uri(),
        "HTTP/3 request received"
    );

    // Read the full request body from the H3 stream.
    let mut body_bytes = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        use bytes::Buf;
        body_bytes.extend_from_slice(chunk.chunk());
    }

    let body_data = Bytes::from(body_bytes);

    // Route through the Salvo service by building a Salvo-compatible request.
    let response = route_h3_via_salvo(req, body_data, client_addr, &state).await;

    // Send the response status + headers.
    let (resp_parts, resp_body) = response.into_parts();
    let resp_head = Response::from_parts(resp_parts, ());
    stream.send_response(resp_head).await?;

    // Send the response body.
    let collected = resp_body
        .collect()
        .await
        .map_err(|e| std::io::Error::other(format!("body collect error: {e}")))?;
    let resp_bytes = collected.to_bytes();

    if !resp_bytes.is_empty() {
        stream.send_data(resp_bytes).await?;
    }

    // Finalize the stream.
    stream.finish().await?;

    Ok(())
}

/// Route an HTTP/3 request through the Salvo service.
///
/// Constructs a Salvo Request from the H3 request parts and body,
/// runs it through the Salvo Service, and returns the response.
async fn route_h3_via_salvo(
    req: Request<()>,
    body_data: Bytes,
    client_addr: SocketAddr,
    state: &AppState,
) -> Response<Body> {
    use salvo::http::ReqBody;

    let service = state.service.load();

    // Build a hyper request with the collected body.
    let (parts, _) = req.into_parts();
    let req_body: ReqBody = ReqBody::Once(body_data);
    let hyper_req = hyper::Request::from_parts(parts, req_body);

    // Use the same hyper_handler path as HTTP/1+2.
    let local_addr: SocketAddr = ([0, 0, 0, 0], 443).into();
    let https_port = state.config.load().global.https_addr.port();
    let alt_svc_h3 = format!("h3=\":{https_port}\"; ma=2592000").parse().ok();

    let handler = service.hyper_handler(
        local_addr.into(),
        client_addr.into(),
        salvo::http::uri::Scheme::HTTPS,
        None,
        alt_svc_h3,
    );

    use hyper::service::Service as HyperService;
    let hyper_resp = match handler.call(hyper_req).await {
        Ok(resp) => resp,
        Err(_) => hyper::Response::builder()
            .status(http::StatusCode::INTERNAL_SERVER_ERROR)
            .body(salvo::http::ResBody::None)
            .unwrap(),
    };

    let (parts, res_body) = hyper_resp.into_parts();
    let body: Body = res_body
        .map_err(|e| -> crate::BoxError { Box::new(e) })
        .boxed();
    Response::from_parts(parts, body)
}
