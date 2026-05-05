//! CGI (Common Gateway Interface) handler.
//!
//! Executes a CGI script as a subprocess, setting the standard CGI environment
//! variables, piping the request body to stdin, and parsing the script's stdout
//! as a CGI-style response (headers followed by body).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use http::{Response, StatusCode};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

use crate::{Body, ProxyError, full_body, goals};

/// CGI handler: executes scripts rooted at a given directory.
pub struct CgiHandler {
    root: PathBuf,
    /// Extra environment variables injected into every CGI invocation.
    env: HashMap<String, String>,
}

impl CgiHandler {
    pub fn new(root: String, env: HashMap<String, String>) -> Self {
        Self {
            root: PathBuf::from(root),
            env,
        }
    }
}

#[salvo::async_trait]
impl salvo::Handler for CgiHandler {
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

impl CgiHandler {
    async fn run(
        &self,
        request: http::Request<crate::Body>,
        client_addr: std::net::SocketAddr,
    ) -> Result<Response<crate::Body>, ProxyError> {
        let path = request.uri().path().to_string();
        let script_path = match resolve_script_path(&self.root, &path) {
            Ok(script_path) => script_path,
            Err(CgiPathError::NotFound) => {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(full_body("Not Found"))?);
            }
            Err(CgiPathError::Forbidden) => {
                return Ok(Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(full_body("Forbidden"))?);
            }
        };

        // Collect the request body before decomposing the request.
        let (parts, body) = request.into_parts();
        let body_bytes = body
            .collect()
            .await
            .map_err(|e| ProxyError::Internal(format!("body collect: {e}")))?
            .to_bytes();

        // Build the child process with CGI environment variables.
        let mut cmd = Command::new(&script_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Standard CGI environment variables.
        cmd.env("REQUEST_METHOD", parts.method.as_str());
        cmd.env("QUERY_STRING", parts.uri.query().unwrap_or(""));
        cmd.env("CONTENT_LENGTH", body_bytes.len().to_string());
        cmd.env(
            "CONTENT_TYPE",
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );
        cmd.env("SERVER_PROTOCOL", format!("{:?}", parts.version));
        cmd.env("SERVER_SOFTWARE", "gatel");
        cmd.env("GATEWAY_INTERFACE", "CGI/1.1");
        cmd.env("SCRIPT_NAME", &path);
        cmd.env("SCRIPT_FILENAME", script_path.to_string_lossy().to_string());
        cmd.env("REQUEST_URI", parts.uri.to_string());
        cmd.env("PATH_INFO", &path);
        cmd.env("REMOTE_ADDR", client_addr.ip().to_string());
        cmd.env("REMOTE_PORT", client_addr.port().to_string());

        if let Some(host) = parts.headers.get("host").and_then(|v| v.to_str().ok()) {
            cmd.env("SERVER_NAME", host.split(':').next().unwrap_or(host));
            if let Some(port) = host.split(':').nth(1) {
                cmd.env("SERVER_PORT", port);
            }
        }

        // Translate HTTP headers to HTTP_* environment variables.
        for (name, value) in &parts.headers {
            if let Ok(v) = value.to_str() {
                let env_name = format!("HTTP_{}", name.as_str().to_uppercase().replace('-', "_"));
                cmd.env(&env_name, v);
            }
        }

        // Inject custom environment variables from configuration.
        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| {
            ProxyError::Internal(format!(
                "failed to spawn CGI script {}: {e}",
                script_path.display()
            ))
        })?;

        // Write request body to stdin, then close to signal EOF.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&body_bytes).await.ok();
            drop(stdin);
        }

        let output = child.wait_with_output().await.map_err(|e| {
            ProxyError::Internal(format!(
                "failed to read CGI output from {}: {e}",
                script_path.display()
            ))
        })?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                script = %script_path.display(),
                stderr = %stderr,
                "CGI script wrote to stderr"
            );
        }

        parse_cgi_response(&output.stdout)
    }
}

// ---------------------------------------------------------------------------
// CGI path resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CgiPathError {
    NotFound,
    Forbidden,
}

fn resolve_script_path(root: &Path, uri_path: &str) -> Result<PathBuf, CgiPathError> {
    let decoded_path = percent_decode_path(uri_path).ok_or(CgiPathError::Forbidden)?;
    if has_forbidden_path_components(Path::new(decoded_path.trim_start_matches('/'))) {
        return Err(CgiPathError::Forbidden);
    }

    let candidate = root.join(uri_path.trim_start_matches('/'));
    let root = root.canonicalize().map_err(|_| CgiPathError::NotFound)?;
    let script = candidate
        .canonicalize()
        .map_err(|_| CgiPathError::NotFound)?;

    if !script.starts_with(&root) {
        return Err(CgiPathError::Forbidden);
    }
    if !script.is_file() {
        return Err(CgiPathError::NotFound);
    }

    Ok(script)
}

fn has_forbidden_path_components(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn percent_decode_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_value(bytes[i + 1])?;
            let lo = hex_value(bytes[i + 2])?;
            output.push((hi << 4) | lo);
            i += 3;
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CGI response parsing (shared with SCGI)
// ---------------------------------------------------------------------------

/// Parse the raw CGI output (headers `\r\n\r\n` or `\n\n` separated from body)
/// into an HTTP response.
///
/// The output has the form:
/// ```text
/// Status: 200 OK\r\n
/// Content-Type: text/html\r\n
/// \r\n
/// <html>...</html>
/// ```
///
/// The `Status` pseudo-header is consumed and used to set the response status.
/// All other headers are forwarded verbatim.  If `Status` is absent, 200 is assumed.
pub fn parse_cgi_response(output: &[u8]) -> Result<Response<Body>, ProxyError> {
    // Prefer \r\n\r\n as the separator; fall back to \n\n for lenient CGI scripts.
    let (header_bytes, body_bytes) = if let Some(pos) = find_subsequence(output, b"\r\n\r\n") {
        (&output[..pos], &output[pos + 4..])
    } else if let Some(pos) = find_subsequence(output, b"\n\n") {
        (&output[..pos], &output[pos + 2..])
    } else {
        // No separator found — treat the whole output as a body with no headers.
        (&[] as &[u8], output)
    };

    let header_str = String::from_utf8_lossy(header_bytes);
    let mut status = StatusCode::OK;
    let mut builder = Response::builder();

    for line in header_str.lines() {
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
                    status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
                }
            } else {
                // Forward any other header to the response.
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
        .map_err(|e| ProxyError::Internal(format!("failed to build CGI response: {e}")))
}

/// Find the first occurrence of `needle` in `haystack`, returning its start index.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cgi_response_with_status() {
        let data = b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nNot here";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
    }

    #[test]
    fn test_parse_cgi_response_default_status() {
        let data = b"Content-Type: text/html\r\n\r\n<h1>Hello</h1>";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn test_parse_cgi_response_lf_separator() {
        // Some CGI scripts use bare \n\n instead of \r\n\r\n.
        let data = b"Content-Type: text/plain\n\nHello";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn test_parse_cgi_response_no_headers() {
        let data = b"just a body with no headers";
        let resp = parse_cgi_response(data).unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn resolve_script_path_allows_files_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.cgi");
        std::fs::write(&script, "echo ok").unwrap();

        let resolved = resolve_script_path(dir.path(), "/run.cgi").unwrap();

        assert_eq!(resolved, script.canonicalize().unwrap());
    }

    #[test]
    fn resolve_script_path_rejects_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().parent().unwrap().join("outside-gatel-cgi-test");
        std::fs::write(&outside, "echo outside").unwrap();

        let result = resolve_script_path(dir.path(), "/../outside-gatel-cgi-test");
        let encoded = resolve_script_path(dir.path(), "/%2e%2e/outside-gatel-cgi-test");

        std::fs::remove_file(&outside).unwrap();
        assert_eq!(result, Err(CgiPathError::Forbidden));
        assert_eq!(encoded, Err(CgiPathError::Forbidden));
    }
}
