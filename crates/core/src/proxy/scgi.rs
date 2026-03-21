//! SCGI (Simple Common Gateway Interface) handler.
//!
//! SCGI is a simpler alternative to FastCGI. The request is encoded as a
//! netstring (length-prefixed) of NUL-separated key-value pairs followed by a
//! comma and the raw request body. The response is parsed as a CGI-style
//! response (headers separated from body by `\r\n\r\n`).
//!
//! Reference: <https://python.ca/scgi/protocol.txt>

use std::collections::HashMap;

use http::Response;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::{Body, ProxyError, goals};

/// SCGI handler: forwards requests to an SCGI server.
pub struct ScgiHandler {
    /// Address of the SCGI server, e.g. `"127.0.0.1:9000"`.
    addr: String,
    /// Extra environment variables injected into every request.
    env: HashMap<String, String>,
}

impl ScgiHandler {
    pub fn new(addr: String, env: HashMap<String, String>) -> Self {
        Self { addr, env }
    }
}

#[salvo::async_trait]
impl salvo::Handler for ScgiHandler {
    async fn handle(
        &self,
        req: &mut salvo::Request,
        _depot: &mut salvo::Depot,
        res: &mut salvo::Response,
        ctrl: &mut salvo::FlowCtrl,
    ) {
        let client_addr = crate::hoops::client_addr(req);
        let request = match goals::strip_request(req) {
            Ok(r) => r,
            Err(e) => {
                goals::merge_response(res, e.into_response());
                ctrl.skip_rest();
                return;
            }
        };
        let response = self
            .run(request, client_addr)
            .await
            .unwrap_or_else(|e| e.into_response());
        goals::merge_response(res, response);
        ctrl.skip_rest();
    }
}

impl ScgiHandler {
    async fn run(
        &self,
        request: http::Request<Body>,
        client_addr: std::net::SocketAddr,
    ) -> Result<Response<Body>, ProxyError> {
        let (parts, body) = request.into_parts();
        let body_bytes = body
            .collect()
            .await
            .map_err(|e| ProxyError::Internal(format!("body collect: {e}")))?
            .to_bytes();

        // Build the SCGI headers buffer (NUL-separated key\0value\0 pairs).
        // Per the SCGI spec, CONTENT_LENGTH must appear first, followed by
        // the SCGI marker, then all other parameters.
        let mut headers: Vec<u8> = Vec::new();
        push_scgi_header(
            &mut headers,
            "CONTENT_LENGTH",
            &body_bytes.len().to_string(),
        );
        push_scgi_header(&mut headers, "SCGI", "1");
        push_scgi_header(&mut headers, "REQUEST_METHOD", parts.method.as_str());
        push_scgi_header(&mut headers, "REQUEST_URI", &parts.uri.to_string());
        push_scgi_header(
            &mut headers,
            "QUERY_STRING",
            parts.uri.query().unwrap_or(""),
        );
        push_scgi_header(
            &mut headers,
            "SERVER_PROTOCOL",
            &format!("{:?}", parts.version),
        );
        push_scgi_header(&mut headers, "REMOTE_ADDR", &client_addr.ip().to_string());
        push_scgi_header(&mut headers, "REMOTE_PORT", &client_addr.port().to_string());
        push_scgi_header(&mut headers, "SERVER_SOFTWARE", "gatel");
        push_scgi_header(&mut headers, "GATEWAY_INTERFACE", "CGI/1.1");

        let path = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| parts.uri.path().to_string());
        push_scgi_header(&mut headers, "SCRIPT_NAME", parts.uri.path());
        push_scgi_header(&mut headers, "PATH_INFO", parts.uri.path());
        push_scgi_header(&mut headers, "DOCUMENT_URI", &path);

        if let Some(ct) = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            push_scgi_header(&mut headers, "CONTENT_TYPE", ct);
        }

        if let Some(host) = parts.headers.get("host").and_then(|v| v.to_str().ok()) {
            push_scgi_header(
                &mut headers,
                "SERVER_NAME",
                host.split(':').next().unwrap_or(host),
            );
            if let Some(port) = host.split(':').nth(1) {
                push_scgi_header(&mut headers, "SERVER_PORT", port);
            }
        }

        // Translate HTTP headers to HTTP_* environment variables.
        for (name, value) in &parts.headers {
            if let Ok(v) = value.to_str() {
                let env_name = format!("HTTP_{}", name.as_str().to_uppercase().replace('-', "_"));
                push_scgi_header(&mut headers, &env_name, v);
            }
        }

        // Inject custom environment variables.
        for (k, v) in &self.env {
            push_scgi_header(&mut headers, k, v);
        }

        // Build the full SCGI payload:  "<header_len>:<headers>,<body>"
        let header_len = headers.len();
        let mut payload = format!("{header_len}:").into_bytes();
        payload.extend_from_slice(&headers);
        payload.push(b',');
        payload.extend_from_slice(&body_bytes);

        debug!(addr = %self.addr, "connecting to SCGI server");

        // Connect, send, and read the full response.
        let mut stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| ProxyError::Internal(format!("SCGI connect to {}: {e}", self.addr)))?;

        stream.write_all(&payload).await.map_err(ProxyError::Io)?;
        stream.flush().await.map_err(ProxyError::Io)?;

        let mut response_buf = Vec::new();
        stream
            .read_to_end(&mut response_buf)
            .await
            .map_err(ProxyError::Io)?;

        // Parse the response using the shared CGI response parser.
        crate::proxy::cgi::parse_cgi_response(&response_buf)
    }
}

// ---------------------------------------------------------------------------
// SCGI helpers
// ---------------------------------------------------------------------------

/// Append a single `name\0value\0` pair to the SCGI headers buffer.
fn push_scgi_header(buf: &mut Vec<u8>, name: &str, value: &str) {
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_scgi_header() {
        let mut buf = Vec::new();
        push_scgi_header(&mut buf, "CONTENT_LENGTH", "0");
        // "CONTENT_LENGTH" (14) + NUL (1) + "0" (1) + NUL (1) = 17 bytes
        assert_eq!(buf.len(), 17);
        assert_eq!(buf[14], 0); // NUL after name
        assert_eq!(buf[15], b'0');
        assert_eq!(buf[16], 0); // NUL after value
    }
}
