use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::info;

/// Format a log line by replacing known placeholders with their values.
fn format_log_line(
    format: &str,
    client_addr: &std::net::SocketAddr,
    method: &str,
    path: &str,
    path_and_query: &str,
    status: u16,
    latency: std::time::Duration,
    headers: &http::HeaderMap,
    content_length: Option<u64>,
) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Build a simple UTC timestamp in "2006-01-02T15:04:05Z" format.
    let timestamp = {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Rough calendar conversion (no external dependency).
        let mut s = secs;
        let sec = s % 60;
        s /= 60;
        let min = s % 60;
        s /= 60;
        let hour = s % 24;
        s /= 24;
        // Days since epoch → year/month/day.
        let mut year = 1970u32;
        loop {
            let days_in_year = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                366
            } else {
                365
            };
            if s < days_in_year {
                break;
            }
            s -= days_in_year;
            year += 1;
        }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days_in_month: [u64; 12] = [
            31,
            if leap { 29u64 } else { 28u64 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 0u32;
        for (i, &dim) in days_in_month.iter().enumerate() {
            if s < dim {
                month = i as u32 + 1;
                break;
            }
            s -= dim;
        }
        let day = s + 1;
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, hour, min, sec
        )
    };

    let host = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    let user_agent = headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    let content_length_str = content_length
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());

    let mut result = format.to_string();
    result = result.replace("{client_ip}", &client_addr.ip().to_string());
    result = result.replace("{client_port}", &client_addr.port().to_string());
    result = result.replace("{method}", method);
    result = result.replace("{path}", path);
    result = result.replace("{path_and_query}", path_and_query);
    result = result.replace("{status}", &status.to_string());
    result = result.replace("{latency_ms}", &latency.as_millis().to_string());
    result = result.replace("{host}", host);
    result = result.replace("{timestamp}", &timestamp);
    result = result.replace("{content_length}", &content_length_str);
    result = result.replace("{user_agent}", user_agent);

    // Handle {header:Name} placeholders.
    while let Some(start) = result.find("{header:") {
        let end = match result[start..].find('}') {
            Some(e) => start + e,
            None => break,
        };
        let placeholder = &result[start..=end];
        let header_name = &placeholder[8..placeholder.len() - 1];
        let value = headers
            .get(header_name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        result = result.replace(placeholder, value);
    }

    result
}

const DEFAULT_FORMAT: &str =
    r#"{client_ip} - [{timestamp}] "{method} {path}" {status} {latency_ms}ms"#;

// ---------------------------------------------------------------------------
// RotatingLogWriter
// ---------------------------------------------------------------------------

/// A buffered file writer that rotates the log file when it exceeds `max_size`.
///
/// When rotation occurs:
/// 1. The current file is flushed and closed.
/// 2. Existing rotated files are renamed: `log.N` → `log.N+1`, `log` → `log.1`.
/// 3. Files beyond `max_keep` are deleted.
/// 4. A new empty file is opened at the original path.
pub struct RotatingLogWriter {
    path: PathBuf,
    writer: tokio::io::BufWriter<tokio::fs::File>,
    current_size: u64,
    max_size: Option<u64>,
    max_keep: Option<usize>,
}

impl RotatingLogWriter {
    /// Open (or create) the log file at `path`.
    pub async fn open(
        path: PathBuf,
        max_size: Option<u64>,
        max_keep: Option<usize>,
    ) -> std::io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let current_size = file.metadata().await?.len();
        Ok(Self {
            path,
            writer: tokio::io::BufWriter::new(file),
            current_size,
            max_size,
            max_keep,
        })
    }

    /// Write bytes to the log, rotating if necessary.
    pub async fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        // Check whether rotation is needed before writing.
        if let Some(max) = self.max_size {
            if self.current_size + data.len() as u64 > max {
                self.rotate().await?;
            }
        }

        self.writer.write_all(data).await?;
        self.current_size += data.len() as u64;
        Ok(())
    }

    /// Flush the writer.
    pub async fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush().await
    }

    /// Perform log rotation synchronously (sync rename is fine since this is infrequent).
    async fn rotate(&mut self) -> std::io::Result<()> {
        // Flush the current file before rotating.
        self.writer.flush().await?;

        // Determine how many rotated files to keep.
        let keep = self.max_keep.unwrap_or(usize::MAX);

        // Shift existing rotated files: log.N → log.N+1
        // We work backwards from the largest existing index down to 1.
        if keep > 0 {
            // Find the highest existing index.
            let mut highest = 0usize;
            for n in 1..=keep {
                let candidate = rotated_path(&self.path, n);
                if candidate.exists() {
                    highest = n;
                } else {
                    break;
                }
            }

            // Rename from highest down to 1, then delete if beyond keep.
            for n in (1..=highest).rev() {
                let src = rotated_path(&self.path, n);
                if n + 1 > keep {
                    // Beyond the keep limit — delete.
                    let _ = std::fs::remove_file(&src);
                } else {
                    let dst = rotated_path(&self.path, n + 1);
                    let _ = std::fs::rename(&src, &dst);
                }
            }

            // Rename the current log → log.1
            if keep >= 1 {
                let dst = rotated_path(&self.path, 1);
                let _ = std::fs::rename(&self.path, &dst);
            }
        }

        // Open a fresh file.
        let new_file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .await?;
        self.writer = tokio::io::BufWriter::new(new_file);
        self.current_size = 0;

        Ok(())
    }
}

/// Compute the rotated path for index `n`, e.g. `/var/log/access.log.1`.
fn rotated_path(base: &PathBuf, n: usize) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{}", n));
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// LoggingHoop
// ---------------------------------------------------------------------------

/// Access-log middleware. Logs method, path, status, and latency.
///
/// Optionally writes structured access logs to a file in addition to the
/// tracing output. Use `LoggingHoop::with_rotating_writer` to enable
/// file output, or `LoggingHoop::with_files` to write errors to a
/// separate file.
pub struct LoggingHoop {
    log_file: Option<Arc<Mutex<RotatingLogWriter>>>,
    error_log_file: Option<Arc<Mutex<RotatingLogWriter>>>,
    format: Option<String>,
}

impl LoggingHoop {
    pub fn new() -> Self {
        Self {
            log_file: None,
            error_log_file: None,
            format: None,
        }
    }

    pub fn with_rotating_writer(
        writer: Arc<Mutex<RotatingLogWriter>>,
        format: Option<String>,
    ) -> Self {
        Self {
            log_file: Some(writer),
            error_log_file: None,
            format,
        }
    }

    /// Create a `LoggingHoop` with separate access and error log writers.
    pub fn with_files(
        access_writer: Arc<Mutex<RotatingLogWriter>>,
        error_writer: Arc<Mutex<RotatingLogWriter>>,
        format: Option<String>,
    ) -> Self {
        Self {
            log_file: Some(access_writer),
            error_log_file: Some(error_writer),
            format,
        }
    }

    /// Convenience constructor that wraps a plain BufWriter<File>.
    pub async fn with_file_path(path: PathBuf, format: Option<String>) -> std::io::Result<Self> {
        let writer = RotatingLogWriter::open(path, None, None).await?;
        Ok(Self {
            log_file: Some(Arc::new(Mutex::new(writer))),
            error_log_file: None,
            format,
        })
    }
}

impl Default for LoggingHoop {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl salvo::Handler for LoggingHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| path.clone());
        let client = super::client_addr(req);
        let req_headers = req.headers().clone();
        let start = Instant::now();

        ctrl.call_next(req, depot, res).await;

        let elapsed = start.elapsed();
        let status = res.status_code.map(|s| s.as_u16()).unwrap_or(200);
        let content_length = res
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        info!(
            client = %client,
            method = %method,
            path = %path,
            status = status,
            latency_ms = elapsed.as_millis() as u64,
            "request handled"
        );

        if let Some(ref file) = self.log_file {
            let fmt = self.format.as_deref().unwrap_or(DEFAULT_FORMAT);
            let line = format_log_line(
                fmt,
                &client,
                method.as_str(),
                &path,
                &path_and_query,
                status,
                elapsed,
                &req_headers,
                content_length,
            );
            let mut writer = file.lock().await;
            let _ = writer.write(line.as_bytes()).await;
            let _ = writer.write(b"\n").await;
            let _ = writer.flush().await;
        }
    }
}
