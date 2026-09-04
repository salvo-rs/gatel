//! Where a log line goes.
//!
//! An open file is a process resource, like a socket: two generations that both
//! log to the same path must share one appender, or they would interleave
//! half-written lines and rotate out from under each other. So sinks live in a
//! table keyed by path, and a reload that keeps a destination keeps the handle,
//! the file and the rotation state.
//!
//! Writing happens on a thread of its own, behind a bounded queue. Nothing on
//! the request path may block on a disk — a gateway that stops serving because
//! a log volume is slow is worse than one that serves without a log line — so a
//! full queue drops, and says how often it had to.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::instrument::Counter;

/// How many lines may be waiting to be written.
///
/// Enough to ride out a slow flush at a few thousand requests a second, small
/// enough that the memory is bounded and obvious.
const QUEUE: usize = 8192;

/// How long the writer waits for more before flushing what it has.
const FLUSH_AFTER: Duration = Duration::from_millis(200);

/// When to start a new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotate {
    /// Bytes, or zero for never — which means something else is managing the
    /// file, and an operator has said so rather than found out later.
    pub size: u64,
    /// How many previous files to keep, `access.log.1` through `.keep`.
    pub keep: usize,
}

impl Rotate {
    pub const NEVER: Self = Self { size: 0, keep: 0 };
}

/// Somewhere lines are written.
#[derive(Debug)]
pub struct Sink {
    name: String,
    /// `None` only while dropping, so the writer thread can be joined after the
    /// queue closes.
    queue: Option<SyncSender<Vec<u8>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    dropped: Counter,
}

impl Sink {
    /// Queue one line. Never blocks, never fails visibly.
    pub fn write(&self, line: &str) {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        let Some(queue) = &self.queue else {
            return;
        };
        match queue.try_send(bytes) {
            Ok(()) => {}
            // A dropped line is a line, not a request. The count is exported so
            // the loss is visible rather than silent.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.increment();
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // Closing the queue is what tells the writer to flush and stop; joining
        // is what stops the process exiting with lines still in memory.
        self.queue.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Every sink the process has open, keyed by where it writes.
///
/// Weak references: a sink lives exactly as long as the generations using it.
/// When the last one is dropped the writer flushes and stops, and the entry is
/// cleared the next time the table is touched.
#[derive(Debug, Default)]
pub struct Sinks {
    open: Mutex<HashMap<String, Weak<Sink>>>,
}

impl Sinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines go to standard error, which is where a container reads them.
    pub fn stderr(&self) -> Arc<Sink> {
        self.resolve("stderr", |name| Ok(spawn(name, Destination::Stderr)))
    }

    /// Lines go to a file, appended and rotated by size.
    ///
    /// The file is opened here, during prepare, so a path that cannot be
    /// written stops the reload rather than failing at the first request.
    pub fn file(&self, path: &Path, rotate: Rotate) -> Result<Arc<Sink>, String> {
        let path = path.to_path_buf();
        let name = label(&path);
        if let Some(existing) = self.get(&name) {
            return Ok(existing);
        }
        let file = open(&path)?;
        Ok(self.resolve(&name, move |name| {
            Ok(spawn(
                name,
                Destination::File {
                    file,
                    path: path.clone(),
                    rotate,
                    written: 0,
                },
            ))
        }))
    }

    fn get(&self, name: &str) -> Option<Arc<Sink>> {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .and_then(Weak::upgrade)
    }

    fn resolve(&self, name: &str, create: impl FnOnce(&str) -> Result<Sink, String>) -> Arc<Sink> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = open.get(name).and_then(Weak::upgrade) {
            return existing;
        }
        let sink = Arc::new(create(name).expect("stderr cannot fail to open"));
        open.insert(name.to_string(), Arc::downgrade(&sink));
        sink
    }

    /// Every sink still in use, for reporting.
    pub fn snapshot(&self) -> Vec<Arc<Sink>> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.retain(|_, sink| sink.strong_count() > 0);
        let mut sinks: Vec<Arc<Sink>> = open.values().filter_map(Weak::upgrade).collect();
        sinks.sort_by(|left, right| left.name.cmp(&right.name));
        sinks
    }
}

enum Destination {
    Stderr,
    File {
        file: File,
        path: PathBuf,
        rotate: Rotate,
        written: u64,
    },
}

impl Destination {
    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        match self {
            Destination::Stderr => std::io::stderr().write_all(line),
            Destination::File {
                file,
                path,
                rotate,
                written,
            } => {
                file.write_all(line)?;
                *written += line.len() as u64;
                if rotate.size > 0 && *written >= rotate.size {
                    *file = roll(path, *rotate)?;
                    *written = 0;
                }
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Destination::Stderr => std::io::stderr().flush(),
            Destination::File { file, .. } => file.flush(),
        }
    }
}

/// How a path is named in a metric label and in the table.
///
/// One separator, whichever platform this is: a dashboard written against a
/// deployment should not have to know, and two spellings of one path are one
/// file however it was typed.
fn label(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn open(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create `{}`: {error}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("cannot write `{}`: {error}", path.display()))
}

/// Shift the kept files along and start a new one.
fn roll(path: &Path, rotate: Rotate) -> std::io::Result<File> {
    let numbered = |index: usize| {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    };
    // Backwards, so nothing is overwritten before it has been moved.
    if rotate.keep > 0 {
        let _ = std::fs::remove_file(numbered(rotate.keep));
        for index in (1..rotate.keep).rev() {
            let _ = std::fs::rename(numbered(index), numbered(index + 1));
        }
        let _ = std::fs::rename(path, numbered(1));
    } else {
        // Keeping nothing means the file starts again, which is the only way
        // `rotate` with `keep=0` can bound the disk.
        let _ = std::fs::remove_file(path);
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn spawn(name: &str, mut destination: Destination) -> Sink {
    let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE);
    let label = name.to_string();
    let worker = std::thread::Builder::new()
        .name(format!("gatel-log-{label}"))
        .spawn(move || {
            loop {
                match receiver.recv_timeout(FLUSH_AFTER) {
                    Ok(line) => {
                        if destination.write(&line).is_err() {
                            // A disk that will not take a line is not something
                            // this thread can fix, and reporting it into the
                            // same log would be a loop.
                            continue;
                        }
                    }
                    // Nothing arrived: flush, so an idle gateway does not sit on
                    // an unwritten line.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let _ = destination.flush();
                    }
                    // Every handle is gone. Flush what is left and stop.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = destination.flush();
                        return;
                    }
                }
            }
        })
        .expect("a log writer thread");

    Sink {
        name: name.to_string(),
        queue: Some(sender),
        worker: Some(worker),
        dropped: Counter::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gatel-sink-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The writer flushes on a timer, so a test that reads the file has to wait
    /// for it rather than guess.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..50 {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_line_reaches_the_file() {
        let directory = directory("write");
        let path = directory.join("access.log");
        let sinks = Sinks::new();
        let sink = sinks.file(&path, Rotate::NEVER).unwrap();
        sink.write("hello");
        assert!(eventually(|| {
            std::fs::read_to_string(&path).unwrap_or_default() == "hello\n"
        }));
    }

    #[test]
    fn two_generations_logging_to_one_path_share_one_appender() {
        let directory = directory("share");
        let path = directory.join("access.log");
        let sinks = Sinks::new();
        let first = sinks.file(&path, Rotate::NEVER).unwrap();
        let second = sinks.file(&path, Rotate::NEVER).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "one open file, or the two would interleave half-written lines"
        );
    }

    #[test]
    fn a_sink_nothing_uses_any_more_is_forgotten() {
        let directory = directory("forget");
        let sinks = Sinks::new();
        let sink = sinks.file(&directory.join("access.log"), Rotate::NEVER).unwrap();
        assert_eq!(sinks.snapshot().len(), 1);
        drop(sink);
        assert!(
            sinks.snapshot().is_empty(),
            "the writer stops with its last handle"
        );
    }

    #[test]
    fn a_path_that_cannot_be_written_stops_the_reload() {
        let directory = directory("refuse");
        // A directory is not a file, so opening it for appending must fail.
        let error = Sinks::new().file(&directory, Rotate::NEVER).unwrap_err();
        assert!(error.contains("cannot write"), "{error}");
    }

    #[test]
    fn a_missing_directory_is_created_rather_than_refused() {
        let directory = directory("nested");
        let path = directory.join("deep/inside/access.log");
        let sinks = Sinks::new();
        let sink = sinks.file(&path, Rotate::NEVER).unwrap();
        sink.write("line");
        assert!(eventually(|| path.exists()));
    }

    #[test]
    fn rotation_keeps_the_previous_files_and_starts_again() {
        let directory = directory("rotate");
        let path = directory.join("access.log");
        let sinks = Sinks::new();
        let sink = sinks.file(&path, Rotate { size: 16, keep: 2 }).unwrap();
        for index in 0..8 {
            sink.write(&format!("line-{index:02}"));
        }
        drop(sink);

        assert!(path.exists(), "a new file is always open");
        assert!(path.with_extension("log.1").exists(), "the previous one is kept");
        assert!(path.with_extension("log.2").exists());
        assert!(
            !path.with_extension("log.3").exists(),
            "keep=2 means two, however many rotations happened"
        );
    }

    #[test]
    fn a_path_is_named_the_same_way_on_every_platform() {
        assert_eq!(
            label(&PathBuf::from("a").join("b").join("access.log")),
            "a/b/access.log"
        );
    }

    #[test]
    fn a_full_queue_drops_lines_rather_than_blocking_a_request() {
        let directory = directory("drop");
        let sinks = Sinks::new();
        let sink = sinks.file(&directory.join("access.log"), Rotate::NEVER).unwrap();
        // Far more than the queue holds, faster than a disk can take them.
        for index in 0..(QUEUE * 4) {
            sink.write(&format!("{index}"));
        }
        // Dropping is allowed; blocking is not. Either the disk kept up or the
        // loss was counted, and never a stalled caller.
        assert!(sink.dropped() < (QUEUE * 4) as u64);
    }
}
