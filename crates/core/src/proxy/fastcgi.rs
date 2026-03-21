//! FastCGI transport for proxying to PHP-FPM and similar FastCGI servers.
//!
//! Implements the FastCGI protocol per the specification:
//! <https://fastcgi-archives.github.io/FastCGI_Specification.html>

use std::collections::HashMap;

use bytes::{BufMut, BytesMut};
use http::Response;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::{Body, ProxyError, full_body, goals};

// ---------------------------------------------------------------------------
// FastCGI protocol constants
// ---------------------------------------------------------------------------

const FCGI_VERSION_1: u8 = 1;
const FCGI_BEGIN_REQUEST: u8 = 1;
#[allow(dead_code)]
const FCGI_ABORT_REQUEST: u8 = 2;
const FCGI_END_REQUEST: u8 = 3;
const FCGI_PARAMS: u8 = 4;
const FCGI_STDIN: u8 = 5;
const FCGI_STDOUT: u8 = 6;
const FCGI_STDERR: u8 = 7;

const FCGI_RESPONDER: u16 = 1;
const FCGI_HEADER_LEN: usize = 8;
const FCGI_REQUEST_ID: u16 = 1;

// Maximum content length in a single FastCGI record.
const FCGI_MAX_CONTENT_LEN: usize = 65535;

// ---------------------------------------------------------------------------
// FastCGI Transport
// ---------------------------------------------------------------------------

/// FastCGI transport: connects to a FastCGI server (e.g. PHP-FPM) and
/// translates HTTP requests into the FastCGI wire protocol.
pub struct FastCgiTransport {
    /// Address of the FastCGI server: `"127.0.0.1:9000"` for TCP.
    addr: String,
    /// Document root on the FastCGI server, e.g. `"/var/www/html"`.
    script_root: String,
    /// Index filenames tried when the URI maps to a directory.
    index: Vec<String>,
    /// Path-info split marker, e.g. `".php"`.  The portion of the URI after
    /// the first occurrence of this suffix becomes PATH_INFO.
    split_path: Option<String>,
    /// Extra environment variables injected into every request.
    env: HashMap<String, String>,
}

impl FastCgiTransport {
    /// Create a new FastCGI transport from configuration values.
    pub fn new(
        addr: String,
        script_root: String,
        index: Vec<String>,
        split_path: Option<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            addr,
            script_root,
            index,
            split_path,
            env,
        }
    }

    /// Send an HTTP request to the FastCGI server and return the response.
    pub async fn send_request(
        &self,
        req: &http::request::Parts,
        body: &[u8],
    ) -> Result<Response<Body>, ProxyError> {
        // Connect to the FastCGI server via TCP.
        let mut stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| ProxyError::Internal(format!("FastCGI connect to {}: {e}", self.addr)))?;

        debug!(addr = %self.addr, "connected to FastCGI server");

        // 1. Send BEGIN_REQUEST
        let begin_body = build_begin_request_body(FCGI_RESPONDER, 0);
        let begin_record = build_record(FCGI_BEGIN_REQUEST, FCGI_REQUEST_ID, &begin_body);
        stream.write_all(&begin_record).await?;

        // 2. Build and send PARAMS
        let params = self.build_params(req, body.len());
        let encoded_params = encode_params(&params);
        send_stream_records(&mut stream, FCGI_PARAMS, FCGI_REQUEST_ID, &encoded_params).await?;
        // Empty PARAMS record to signal end of params.
        let empty_params = build_record(FCGI_PARAMS, FCGI_REQUEST_ID, &[]);
        stream.write_all(&empty_params).await?;

        // 3. Send STDIN (request body)
        send_stream_records(&mut stream, FCGI_STDIN, FCGI_REQUEST_ID, body).await?;
        // Empty STDIN to signal end of input.
        let empty_stdin = build_record(FCGI_STDIN, FCGI_REQUEST_ID, &[]);
        stream.write_all(&empty_stdin).await?;

        stream.flush().await?;

        // 4. Read response records (STDOUT, STDERR, END_REQUEST)
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        loop {
            let header = read_record_header(&mut stream).await?;
            let content = read_exact(&mut stream, header.content_length as usize).await?;
            // Skip padding bytes.
            if header.padding_length > 0 {
                let _padding = read_exact(&mut stream, header.padding_length as usize).await?;
            }

            match header.record_type {
                FCGI_STDOUT => {
                    stdout_buf.extend_from_slice(&content);
                }
                FCGI_STDERR => {
                    stderr_buf.extend_from_slice(&content);
                    if !stderr_buf.is_empty() {
                        let msg = String::from_utf8_lossy(&stderr_buf);
                        warn!(fastcgi_stderr = %msg, "FastCGI stderr output");
                    }
                }
                FCGI_END_REQUEST => {
                    debug!("FastCGI END_REQUEST received");
                    break;
                }
                other => {
                    debug!(record_type = other, "ignoring unknown FastCGI record type");
                }
            }
        }

        // 5. Parse the STDOUT data as an HTTP response. FastCGI STDOUT contains CGI-style headers
        //    followed by \r\n\r\n and the body.
        parse_cgi_response(&stdout_buf)
    }

    /// Build the CGI environment variable map for a request.
    fn build_params(
        &self,
        req: &http::request::Parts,
        content_length: usize,
    ) -> Vec<(String, String)> {
        let uri_path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // Determine SCRIPT_NAME and PATH_INFO based on split_path.
        let (script_name, path_info) = if let Some(ref split) = self.split_path {
            split_script_path(uri_path, split)
        } else {
            (uri_path.to_string(), String::new())
        };

        // If the script name ends with '/' or is a directory, try index files.
        let script_name = if script_name.ends_with('/') || script_name == "/" {
            let idx = self
                .index
                .first()
                .cloned()
                .unwrap_or_else(|| "index.php".into());
            format!(
                "{}{}",
                script_name.trim_end_matches('/'),
                if script_name == "/" {
                    format!("/{idx}")
                } else {
                    format!("/{idx}")
                }
            )
        } else {
            script_name
        };

        let script_filename = format!("{}{}", self.script_root.trim_end_matches('/'), script_name);

        let path_translated = if path_info.is_empty() {
            String::new()
        } else {
            format!("{}{}", self.script_root.trim_end_matches('/'), path_info)
        };

        let server_name = req
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
            .unwrap_or_else(|| "localhost".into());

        let server_port = req
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.split(':').nth(1))
            .unwrap_or("80")
            .to_string();

        let content_type = req
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let server_protocol = format!("{:?}", req.version);

        let request_uri = req
            .uri
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| uri_path.to_string());

        let mut params = vec![
            ("SCRIPT_FILENAME".into(), script_filename),
            ("SCRIPT_NAME".into(), script_name.clone()),
            ("DOCUMENT_ROOT".into(), self.script_root.clone()),
            ("QUERY_STRING".into(), query.to_string()),
            ("REQUEST_METHOD".into(), req.method.to_string()),
            ("CONTENT_TYPE".into(), content_type),
            ("CONTENT_LENGTH".into(), content_length.to_string()),
            ("SERVER_NAME".into(), server_name),
            ("SERVER_PORT".into(), server_port),
            ("SERVER_PROTOCOL".into(), server_protocol),
            ("REQUEST_URI".into(), request_uri),
            ("PATH_INFO".into(), path_info.clone()),
            ("PATH_TRANSLATED".into(), path_translated),
            ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
            ("SERVER_SOFTWARE".into(), "gatel".into()),
        ];

        // Add HTTP headers as HTTP_* environment variables.
        for (name, value) in req.headers.iter() {
            if let Ok(val) = value.to_str() {
                let env_name = format!("HTTP_{}", name.as_str().to_uppercase().replace('-', "_"));
                params.push((env_name, val.to_string()));
            }
        }

        // Add extra env vars from configuration.
        for (k, v) in &self.env {
            params.push((k.clone(), v.clone()));
        }

        params
    }
}

#[salvo::async_trait]
impl salvo::Handler for FastCgiTransport {
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

impl FastCgiTransport {
    async fn run(
        &self,
        request: http::Request<crate::Body>,
        _client_addr: std::net::SocketAddr,
    ) -> Result<http::Response<crate::Body>, crate::ProxyError> {
        use http_body_util::BodyExt;

        let (parts, body) = request.into_parts();
        let body_bytes = body
            .collect()
            .await
            .map_err(|e| ProxyError::Internal(format!("failed to buffer body: {e}")))?
            .to_bytes();

        self.send_request(&parts, &body_bytes).await
    }
}

// ---------------------------------------------------------------------------
// FastCGI record building
// ---------------------------------------------------------------------------

/// Build a raw FastCGI record: header (8 bytes) + content + padding.
fn build_record(record_type: u8, request_id: u16, content: &[u8]) -> Vec<u8> {
    let content_len = content.len() as u16;
    let padding_len = padding_for(content.len());
    let total = FCGI_HEADER_LEN + content.len() + padding_len as usize;

    let mut buf = Vec::with_capacity(total);
    buf.push(FCGI_VERSION_1);
    buf.push(record_type);
    buf.push((request_id >> 8) as u8);
    buf.push(request_id as u8);
    buf.push((content_len >> 8) as u8);
    buf.push(content_len as u8);
    buf.push(padding_len);
    buf.push(0); // reserved
    buf.extend_from_slice(content);
    // Append padding.
    for _ in 0..padding_len {
        buf.push(0);
    }
    buf
}

/// Compute padding to align content to 8-byte boundaries.
fn padding_for(content_len: usize) -> u8 {
    let remainder = content_len % 8;
    if remainder == 0 {
        0
    } else {
        (8 - remainder) as u8
    }
}

/// Build the 8-byte body for a BEGIN_REQUEST record.
fn build_begin_request_body(role: u16, flags: u8) -> [u8; 8] {
    let mut body = [0u8; 8];
    body[0] = (role >> 8) as u8;
    body[1] = role as u8;
    body[2] = flags;
    // bytes 3..7 are reserved
    body
}

/// Encode a list of key-value pairs in FastCGI name-value pair format.
fn encode_params(params: &[(String, String)]) -> Vec<u8> {
    let mut buf = BytesMut::new();
    for (name, value) in params {
        encode_length(&mut buf, name.len());
        encode_length(&mut buf, value.len());
        buf.put_slice(name.as_bytes());
        buf.put_slice(value.as_bytes());
    }
    buf.to_vec()
}

/// Encode a length value per FastCGI spec:
/// - If < 128: single byte
/// - If >= 128: 4 bytes with high bit set on first byte
fn encode_length(buf: &mut BytesMut, len: usize) {
    if len < 128 {
        buf.put_u8(len as u8);
    } else {
        buf.put_u8(((len >> 24) as u8) | 0x80);
        buf.put_u8((len >> 16) as u8);
        buf.put_u8((len >> 8) as u8);
        buf.put_u8(len as u8);
    }
}

/// Send data as one or more FastCGI records (splitting at FCGI_MAX_CONTENT_LEN).
async fn send_stream_records(
    stream: &mut TcpStream,
    record_type: u8,
    request_id: u16,
    data: &[u8],
) -> Result<(), ProxyError> {
    let mut offset = 0;
    while offset < data.len() {
        let end = std::cmp::min(offset + FCGI_MAX_CONTENT_LEN, data.len());
        let chunk = &data[offset..end];
        let record = build_record(record_type, request_id, chunk);
        stream.write_all(&record).await?;
        offset = end;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FastCGI record reading
// ---------------------------------------------------------------------------

struct RecordHeader {
    #[allow(dead_code)]
    version: u8,
    record_type: u8,
    #[allow(dead_code)]
    request_id: u16,
    content_length: u16,
    padding_length: u8,
}

async fn read_record_header(stream: &mut TcpStream) -> Result<RecordHeader, ProxyError> {
    let mut buf = [0u8; FCGI_HEADER_LEN];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| ProxyError::Internal(format!("failed to read FastCGI record header: {e}")))?;

    Ok(RecordHeader {
        version: buf[0],
        record_type: buf[1],
        request_id: u16::from_be_bytes([buf[2], buf[3]]),
        content_length: u16::from_be_bytes([buf[4], buf[5]]),
        padding_length: buf[6],
    })
}

async fn read_exact(stream: &mut TcpStream, len: usize) -> Result<Vec<u8>, ProxyError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.map_err(|e| {
        ProxyError::Internal(format!("failed to read {len} bytes from FastCGI: {e}"))
    })?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// CGI response parsing
// ---------------------------------------------------------------------------

/// Parse the raw CGI output (from FastCGI STDOUT) into an HTTP response.
///
/// The output has the form:
/// ```text
/// Status: 200 OK\r\n
/// Content-Type: text/html\r\n
/// \r\n
/// <html>...</html>
/// ```
///
/// The `Status` header is optional; if absent, 200 is assumed.
fn parse_cgi_response(data: &[u8]) -> Result<Response<Body>, ProxyError> {
    // Find the header/body separator: \r\n\r\n
    let separator = find_subsequence(data, b"\r\n\r\n");
    let (header_bytes, body_bytes) = match separator {
        Some(pos) => (&data[..pos], &data[pos + 4..]),
        None => {
            // No headers found; treat entire output as body.
            (&[] as &[u8], data)
        }
    };

    let header_str = String::from_utf8_lossy(header_bytes);
    let mut status = http::StatusCode::OK;
    let mut builder = Response::builder();

    for line in header_str.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim();
            let value = line[colon_pos + 1..].trim();

            if name.eq_ignore_ascii_case("status") {
                // Parse "200 OK" or just "200".
                let code_str = value.split_whitespace().next().unwrap_or("200");
                if let Ok(code) = code_str.parse::<u16>() {
                    status = http::StatusCode::from_u16(code).unwrap_or(http::StatusCode::OK);
                }
            } else {
                // Add as response header.
                if let (Ok(hn), Ok(hv)) = (
                    name.parse::<http::header::HeaderName>(),
                    value.parse::<http::header::HeaderValue>(),
                ) {
                    builder = builder.header(hn, hv);
                }
            }
        }
    }

    builder = builder.status(status);
    let body = full_body(bytes::Bytes::copy_from_slice(body_bytes));
    builder
        .body(body)
        .map_err(|e| ProxyError::Internal(format!("failed to build FastCGI response: {e}")))
}

/// Find the position of a subsequence in a byte slice.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Split a URI path into (script_name, path_info) based on a split marker.
///
/// For example, with split=".php" and path="/app/index.php/foo/bar":
///   script_name = "/app/index.php"
///   path_info   = "/foo/bar"
fn split_script_path(path: &str, split: &str) -> (String, String) {
    if let Some(pos) = path.find(split) {
        let split_end = pos + split.len();
        let script = &path[..split_end];
        let info = &path[split_end..];
        (script.to_string(), info.to_string())
    } else {
        (path.to_string(), String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_record() {
        let record = build_record(FCGI_BEGIN_REQUEST, 1, &[0; 8]);
        assert_eq!(record[0], FCGI_VERSION_1);
        assert_eq!(record[1], FCGI_BEGIN_REQUEST);
        assert_eq!(record.len(), FCGI_HEADER_LEN + 8); // 8 content, 0 padding
    }

    #[test]
    fn test_encode_length_short() {
        let mut buf = BytesMut::new();
        encode_length(&mut buf, 5);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn test_encode_length_long() {
        let mut buf = BytesMut::new();
        encode_length(&mut buf, 300);
        assert_eq!(buf.len(), 4);
        assert_eq!(buf[0] & 0x80, 0x80); // high bit set
    }

    #[test]
    fn test_parse_cgi_response_basic() {
        let data = b"Status: 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>Hello</h1>";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "text/html");
    }

    #[test]
    fn test_parse_cgi_response_no_status() {
        let data = b"Content-Type: text/plain\r\n\r\nHello";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn test_split_script_path() {
        let (script, info) = split_script_path("/app/index.php/foo/bar", ".php");
        assert_eq!(script, "/app/index.php");
        assert_eq!(info, "/foo/bar");
    }

    #[test]
    fn test_split_script_path_no_match() {
        let (script, info) = split_script_path("/app/style.css", ".php");
        assert_eq!(script, "/app/style.css");
        assert_eq!(info, "");
    }

    #[test]
    fn test_padding_for() {
        assert_eq!(padding_for(0), 0);
        assert_eq!(padding_for(8), 0);
        assert_eq!(padding_for(1), 7);
        assert_eq!(padding_for(10), 6);
    }

    #[test]
    fn test_encode_params() {
        let params = vec![("KEY".to_string(), "val".to_string())];
        let encoded = encode_params(&params);
        // length(3) + length(3) + "KEY" + "val" = 1 + 1 + 3 + 3 = 8
        assert_eq!(encoded.len(), 8);
        assert_eq!(encoded[0], 3); // key length
        assert_eq!(encoded[1], 3); // value length
    }
}
