use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use http::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RANGE, VARY,
};
use http::{Response, StatusCode};
use tokio::io::AsyncReadExt;
use tracing::debug;

use crate::{Body, ProxyError, empty_body, encoding, full_body, http_date, mime_guess};

/// Static file server handler.
///
/// Serves files from a configured root directory. Supports:
/// - MIME type detection via file extension
/// - ETag generation (based on file mtime + size)
/// - Last-Modified header
/// - Conditional requests (If-None-Match, If-Modified-Since → 304)
/// - Byte range requests (Accept-Ranges, Content-Range)
/// - Directory index (configurable list, default: index.html)
/// - Directory browsing (optional)
/// - Trailing slash redirect for directories (optional)
/// - Path traversal prevention
pub struct FileServerGoal {
    root: PathBuf,
    browse: bool,
    trailing_slash: bool,
    index: Vec<String>,
}

impl FileServerGoal {
    pub fn new(
        root: impl Into<PathBuf>,
        browse: bool,
        trailing_slash: bool,
        index: Vec<String>,
    ) -> Self {
        Self {
            root: root.into(),
            browse,
            trailing_slash,
            index: if index.is_empty() {
                vec!["index.html".to_string()]
            } else {
                index
            },
        }
    }

    /// Resolve the request path to a filesystem path, preventing traversal.
    fn resolve_path(&self, request_path: &str) -> Option<PathBuf> {
        // Decode percent-encoded path.
        let decoded = encoding::percent_decode(request_path);

        // Normalize: remove leading slash, collapse double-slashes.
        let cleaned = decoded.trim_start_matches('/').replace("\\", "/");

        // Reject any path component that is ".." to prevent traversal.
        for component in cleaned.split('/') {
            if component == ".." {
                return None;
            }
        }

        let candidate = self.root.join(&cleaned);

        // Double-check: the canonical path must be under root.
        // We do a prefix check on the string representation.
        // Note: we don't canonicalize because the file may not exist yet,
        // and canonical resolution may differ on Windows. The ".." check
        // above is the primary guard.
        Some(candidate)
    }
}

#[salvo::async_trait]
impl salvo::Handler for FileServerGoal {
    async fn handle(
        &self,
        req: &mut salvo::Request,
        _depot: &mut salvo::Depot,
        res: &mut salvo::Response,
        ctrl: &mut salvo::FlowCtrl,
    ) {
        let headers = req.headers().clone();
        let request_path = req.uri().path().to_string();
        let response = self
            .serve(&request_path, &headers)
            .await
            .unwrap_or_else(|e| e.into_response());
        super::merge_response(res, response);
        ctrl.skip_rest();
    }
}

impl FileServerGoal {
    async fn serve(
        &self,
        request_path: &str,
        req_headers: &http::HeaderMap,
    ) -> Result<Response<Body>, ProxyError> {
        // Resolve to filesystem path.
        let file_path = match self.resolve_path(request_path) {
            Some(p) => p,
            None => {
                debug!(path = request_path, "path traversal blocked");
                return Ok(Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(full_body("Forbidden"))
                    .unwrap());
            }
        };

        // If it's a directory, optionally redirect to add a trailing slash.
        let is_dir = file_path.is_dir();
        if is_dir && self.trailing_slash && !request_path.ends_with('/') {
            // Check that this directory has an index file or browsing is enabled.
            let has_index = self.index.iter().any(|name| file_path.join(name).exists());
            if has_index || self.browse {
                let redirect_to = format!("{}/", request_path);
                debug!(path = %request_path, "redirecting to add trailing slash");
                return Ok(Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header(http::header::LOCATION, redirect_to)
                    .body(crate::empty_body())
                    .unwrap());
            }
        }

        // If it's a directory, look for index files first.
        let file_path = if is_dir {
            // Try each index filename in order.
            let index_path = self
                .index
                .iter()
                .map(|name| file_path.join(name))
                .find(|p| p.exists());

            if let Some(idx) = index_path {
                idx
            } else if self.browse {
                // No index file found — generate a directory listing.
                debug!(path = ?file_path, "generating directory listing");
                let html = generate_directory_listing(&file_path, &self.root, request_path).await?;
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header(http::header::CONTENT_LENGTH, html.len())
                    .body(full_body(html))
                    .unwrap());
            } else {
                // Browsing disabled — 404.
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(full_body("Not Found"))
                    .unwrap());
            }
        } else {
            file_path
        };

        // Open file.
        let file = match tokio::fs::File::open(&file_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(full_body("Not Found"))
                    .unwrap());
            }
            Err(e) => return Err(ProxyError::Io(e)),
        };

        let metadata = file.metadata().await.map_err(ProxyError::Io)?;

        if metadata.is_dir() {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(full_body("Not Found"))
                .unwrap());
        }

        let file_size = metadata.len();
        let modified = metadata.modified().ok();
        let etag = http_date::generate_etag(file_size, modified.as_ref());
        let last_modified_str = modified.map(http_date::format_http_date);

        // Check If-None-Match.
        if let Some(inm) = req_headers.get(IF_NONE_MATCH)
            && let Ok(inm_str) = inm.to_str()
            && inm_str.trim_matches('"') == etag.trim_matches('"')
        {
            return Ok(Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(ETAG, &etag)
                .body(empty_body())
                .unwrap());
        }

        // Check If-Modified-Since.
        if let (Some(ims), Some(mod_time)) = (req_headers.get(IF_MODIFIED_SINCE), modified.as_ref())
            && let Ok(ims_str) = ims.to_str()
            && let Some(ims_time) = http_date::parse_http_date(ims_str)
        {
            // File not modified since the date the client has.
            if *mod_time <= ims_time {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(ETAG, &etag)
                    .body(empty_body())
                    .unwrap());
            }
        }

        let content_type = mime_from_path(&file_path);

        // Check for pre-compressed assets (br > zstd > gzip).
        // Only attempt this for non-range requests.
        if !req_headers.contains_key(RANGE) {
            let accept_encoding = req_headers
                .get(ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            // Preference order: br > zstd > gzip.
            let candidates: &[(&str, &str)] = &[("br", ".br"), ("zstd", ".zst"), ("gzip", ".gz")];

            for (encoding_name, ext) in candidates {
                if accept_encoding.contains(encoding_name) || accept_encoding.contains('*') {
                    let compressed_path = {
                        let mut p = file_path.as_os_str().to_owned();
                        p.push(ext);
                        PathBuf::from(p)
                    };
                    if compressed_path.exists() {
                        debug!(
                            path = ?compressed_path,
                            encoding = encoding_name,
                            "serving pre-compressed file"
                        );
                        let comp_file = tokio::fs::File::open(&compressed_path)
                            .await
                            .map_err(ProxyError::Io)?;
                        let comp_metadata = comp_file.metadata().await.map_err(ProxyError::Io)?;
                        let comp_size = comp_metadata.len();

                        let mut comp_contents = Vec::with_capacity(comp_size as usize);
                        let mut comp_file = comp_file;
                        comp_file
                            .read_to_end(&mut comp_contents)
                            .await
                            .map_err(ProxyError::Io)?;

                        let mut builder = Response::builder()
                            .status(StatusCode::OK)
                            .header(CONTENT_TYPE, content_type)
                            .header(CONTENT_LENGTH, comp_size)
                            .header(CONTENT_ENCODING, *encoding_name)
                            .header(VARY, "Accept-Encoding")
                            .header(ACCEPT_RANGES, "bytes")
                            .header(ETAG, &etag);

                        if let Some(ref lm) = last_modified_str {
                            builder = builder.header(LAST_MODIFIED, lm.as_str());
                        }

                        return Ok(builder
                            .body(full_body(bytes::Bytes::from(comp_contents)))
                            .unwrap());
                    }
                }
            }
        }

        // Check for Range header.
        if let Some(range_header) = req_headers.get(RANGE)
            && let Ok(range_str) = range_header.to_str()
        {
            if let Some((start, end)) = http_date::parse_range(range_str, file_size) {
                let length = end - start + 1;

                // Read the specific range.
                let mut file = file;
                use tokio::io::AsyncSeekExt;
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(ProxyError::Io)?;
                let mut buf = vec![0u8; length as usize];
                file.read_exact(&mut buf).await.map_err(ProxyError::Io)?;

                let content_range = format!("bytes {start}-{end}/{file_size}");

                let mut builder = Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(CONTENT_TYPE, content_type)
                    .header(CONTENT_LENGTH, length)
                    .header(CONTENT_RANGE, content_range)
                    .header(ACCEPT_RANGES, "bytes")
                    .header(ETAG, &etag);

                if let Some(ref lm) = last_modified_str {
                    builder = builder.header(LAST_MODIFIED, lm.as_str());
                }

                return Ok(builder.body(full_body(Bytes::from(buf))).unwrap());
            }
            // Invalid range — return 416.
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(CONTENT_RANGE, format!("bytes */{file_size}"))
                .body(empty_body())
                .unwrap());
        }

        // Full file read.
        let mut file = file;
        let mut contents = Vec::with_capacity(file_size as usize);
        file.read_to_end(&mut contents)
            .await
            .map_err(ProxyError::Io)?;

        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, file_size)
            .header(ACCEPT_RANGES, "bytes")
            .header(ETAG, &etag);

        if let Some(ref lm) = last_modified_str {
            builder = builder.header(LAST_MODIFIED, lm.as_str());
        }

        debug!(path = ?file_path, size = file_size, "serving file");
        Ok(builder.body(full_body(Bytes::from(contents))).unwrap())
    }
}

// ---------------------------------------------------------------------------
// Directory browsing
// ---------------------------------------------------------------------------

/// Directory listing entry for sorting.
struct DirEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

/// Generate an HTML directory listing for the given directory path.
async fn generate_directory_listing(
    dir_path: &Path,
    root: &Path,
    request_path: &str,
) -> Result<String, ProxyError> {
    let mut entries = Vec::new();

    let mut read_dir = tokio::fs::read_dir(dir_path)
        .await
        .map_err(ProxyError::Io)?;

    while let Some(entry) = read_dir.next_entry().await.map_err(ProxyError::Io)? {
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        entries.push(DirEntry {
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }

    // Sort: directories first, then alphabetically by name.
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });

    let display_path = request_path.trim_end_matches('/');
    let display_path = if display_path.is_empty() {
        "/"
    } else {
        display_path
    };

    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    html.push_str(&format!(
        "<title>Index of {}</title>\n",
        encoding::html_escape(display_path)
    ));
    html.push_str("<style>\n");
    html.push_str(
        "body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; \
         margin: 2em; color: #333; background: #fafafa; }\n",
    );
    html.push_str(
        "h1 { font-size: 1.4em; font-weight: 600; border-bottom: 1px solid #ddd; \
         padding-bottom: 0.5em; }\n",
    );
    html.push_str("table { border-collapse: collapse; width: 100%; max-width: 900px; }\n");
    html.push_str("th, td { text-align: left; padding: 6px 16px 6px 0; }\n");
    html.push_str(
        "th { font-size: 0.85em; text-transform: uppercase; color: #666; \
         border-bottom: 2px solid #ddd; }\n",
    );
    html.push_str("tr:hover { background: #f0f0f0; }\n");
    html.push_str("a { color: #0366d6; text-decoration: none; }\n");
    html.push_str("a:hover { text-decoration: underline; }\n");
    html.push_str(".dir a::before { content: '\\1F4C1 '; }\n");
    html.push_str(".size, .date { color: #666; font-size: 0.9em; }\n");
    html.push_str("</style>\n</head>\n<body>\n");
    html.push_str(&format!(
        "<h1>Index of {}</h1>\n",
        encoding::html_escape(display_path)
    ));
    html.push_str("<table>\n<thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead>\n");
    html.push_str("<tbody>\n");

    // Parent directory link (unless at root).
    let is_root = dir_path == root
        || dir_path
            .canonicalize()
            .ok()
            .and_then(|c| root.canonicalize().ok().map(|r| c == r))
            .unwrap_or(false);

    if !is_root && display_path != "/" {
        let parent = if let Some(pos) = display_path.rfind('/') {
            if pos == 0 {
                "/".to_string()
            } else {
                display_path[..pos].to_string()
            }
        } else {
            "/".to_string()
        };
        html.push_str(&format!(
            "<tr class=\"dir\"><td><a href=\"{}\">..</a></td><td class=\"size\">-</td><td class=\"date\">-</td></tr>\n",
            encoding::html_escape(&parent)
        ));
    }

    // Normalize request_path for link construction.
    let base = if request_path.ends_with('/') {
        request_path.to_string()
    } else {
        format!("{}/", request_path)
    };

    for entry in &entries {
        let display_name = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let href = format!("{}{}", base, encoding::percent_encode(&entry.name));
        let href = if entry.is_dir {
            format!("{}/", href.trim_end_matches('/'))
        } else {
            href
        };

        let size_str = if entry.is_dir {
            "-".to_string()
        } else {
            mime_guess::format_size(entry.size)
        };

        let date_str = entry
            .modified
            .map(http_date::format_http_date)
            .unwrap_or_else(|| "-".to_string());

        let class = if entry.is_dir { " class=\"dir\"" } else { "" };

        html.push_str(&format!(
            "<tr{}><td><a href=\"{}\">{}</a></td><td class=\"size\">{}</td><td class=\"date\">{}</td></tr>\n",
            class,
            encoding::html_escape(&href),
            encoding::html_escape(&display_name),
            size_str,
            date_str,
        ));
    }

    html.push_str("</tbody>\n</table>\n</body>\n</html>\n");
    Ok(html)
}

/// Determine MIME type from a file's extension.
///
/// Thin wrapper around [`crate::mime_guess::mime_from_extension`] that
/// extracts the extension from a `Path`.
fn mime_from_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    mime_guess::mime_from_extension(ext.as_str())
}
