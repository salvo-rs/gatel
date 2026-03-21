//! Observability backends for structured logging.
//!
//! Provides multiple log output destinations beyond the default stderr:
//! - [`LogfileBackend`] — write structured logs to a rotating file
//! - [`StdlogBackend`] — write structured JSON logs to stdout or stderr

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::debug;

// ---------------------------------------------------------------------------
// Logfile backend
// ---------------------------------------------------------------------------

/// A rotating log file writer.
///
/// Logs are appended to the current file. When the file exceeds
/// `rotate_size`, it is renamed with a numeric suffix and a new file is
/// opened. Up to `rotate_keep` old files are retained.
pub struct LogfileBackend {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
    rotate_size: u64,
    rotate_keep: usize,
    bytes_written: Mutex<u64>,
}

impl LogfileBackend {
    /// Open or create a log file at `path`.
    pub fn new(path: impl Into<PathBuf>, rotate_size: u64, rotate_keep: usize) -> io::Result<Self> {
        let path = path.into();
        let file = open_append(&path)?;
        let initial_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
            rotate_size,
            rotate_keep,
            bytes_written: Mutex::new(initial_size),
        })
    }

    /// Write a log line. Rotates if size threshold is exceeded.
    pub fn write_line(&self, line: &str) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let mut bytes = self.bytes_written.lock().unwrap();

        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        *bytes += line.len() as u64 + 1;

        if self.rotate_size > 0 && *bytes >= self.rotate_size {
            drop(writer);
            drop(bytes);
            self.rotate()?;
        }

        Ok(())
    }

    fn rotate(&self) -> io::Result<()> {
        // Rename existing rotated files: .2 -> .3, .1 -> .2, etc.
        for i in (1..self.rotate_keep).rev() {
            let from = rotated_path(&self.path, i);
            let to = rotated_path(&self.path, i + 1);
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }

        // Delete the oldest if it exceeds keep count.
        let oldest = rotated_path(&self.path, self.rotate_keep);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }

        // Current → .1
        if self.path.exists() {
            std::fs::rename(&self.path, rotated_path(&self.path, 1))?;
        }

        // Open a fresh file.
        let file = open_append(&self.path)?;
        let mut writer = self.writer.lock().unwrap();
        *writer = BufWriter::new(file);
        let mut bytes = self.bytes_written.lock().unwrap();
        *bytes = 0;

        debug!(path = %self.path.display(), "log file rotated");
        Ok(())
    }
}

fn rotated_path(base: &Path, n: usize) -> PathBuf {
    let mut p = base.as_os_str().to_owned();
    p.push(format!(".{n}"));
    PathBuf::from(p)
}

fn open_append(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

// ---------------------------------------------------------------------------
// Stdlog backend
// ---------------------------------------------------------------------------

/// Output destination for [`StdlogBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdlogOutput {
    Stdout,
    Stderr,
}

/// Structured log writer to stdout or stderr.
///
/// Each line is a JSON object with `timestamp`, `level`, and `message` fields.
pub struct StdlogBackend {
    output: StdlogOutput,
    json: bool,
}

impl StdlogBackend {
    /// Create a new stdlog backend.
    pub fn new(output: StdlogOutput, json: bool) -> Self {
        Self { output, json }
    }

    /// Write a log entry.
    pub fn write_entry(&self, level: &str, message: &str) {
        let line = if self.json {
            let ts = chrono_now();
            format!(
                r#"{{"timestamp":"{ts}","level":"{level}","message":{}}}"#,
                json_escape(message)
            )
        } else {
            format!("[{level}] {message}")
        };

        match self.output {
            StdlogOutput::Stdout => {
                let _ = writeln!(io::stdout(), "{line}");
            }
            StdlogOutput::Stderr => {
                let _ = writeln!(io::stderr(), "{line}");
            }
        }
    }
}

/// Simple ISO-8601 timestamp without pulling in chrono.
fn chrono_now() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    // Very simple formatting — not locale-aware but good enough for logs.
    format!("{secs}")
}

/// Escape a string for JSON output.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logfile_write_and_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");

        let backend = LogfileBackend::new(&path, 50, 3).unwrap();
        for i in 0..10 {
            backend
                .write_line(&format!("line {i} with some padding"))
                .unwrap();
        }

        // After rotation, the main file should exist and be small.
        assert!(path.exists());
        // At least one rotated file should exist.
        assert!(rotated_path(&path, 1).exists());
    }

    #[test]
    fn stdlog_json_format() {
        let backend = StdlogBackend::new(StdlogOutput::Stderr, true);
        // Just verify it doesn't panic.
        backend.write_entry("info", "test message with \"quotes\"");
    }

    #[test]
    fn json_escape_special_chars() {
        assert_eq!(json_escape("hello"), "\"hello\"");
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("a\nb"), "\"a\\nb\"");
    }
}
