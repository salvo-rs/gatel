//! Process-owned log writers with bounded request-side queues.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::instrument::Counter;
use crate::sink_io::{IoStats, Operation};
use crate::sink_lifecycle::{Completed, Completion, Queue, Worker};
pub use crate::sink_lifecycle::{Drain, ShutdownReport};
pub use crate::sink_policy::{SinkPublication, SinkUpdate, StagedSinks};

const QUEUE: usize = 8192;
const FLUSH_AFTER: Duration = Duration::from_millis(200);
/// Active and retiring workers share this bound, including stalled OS writes.
pub const MAX_WRITERS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotate {
    pub size: u64,
    pub keep: usize,
}
impl Rotate {
    pub const NEVER: Self = Self { size: 0, keep: 0 };
}

#[derive(Debug)]
pub struct Sink {
    name: String,
    queue: Queue,
    worker: Arc<Worker>,
    dropped: Counter,
    pub(crate) policy: Arc<Mutex<Rotate>>,
    io: Arc<IoStats>,
}

impl Sink {
    /// Enqueue without waiting for the writer or its disk.
    pub fn write(&self, line: &str) {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let Some(queue) = queue.as_ref() else {
            self.dropped.increment();
            return;
        };
        match queue.try_send(bytes) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => self.dropped.increment(),
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }
    pub fn write_errors(&self) -> u64 {
        self.io.write.get()
    }
    pub fn flush_errors(&self) -> u64 {
        self.io.flush.get()
    }
    pub fn rotation_errors(&self) -> u64 {
        self.io.rotate.get()
    }
    pub fn last_success(&self) -> u64 {
        self.io.last_write()
    }
    pub fn io_failed(&self) -> bool {
        self.io.failed()
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.worker.close();
    }
}

#[derive(Debug)]
struct Registered {
    sink: Weak<Sink>,
    io: Weak<IoStats>,
    worker: Arc<Worker>,
}

#[derive(Debug)]
struct Registry {
    open: HashMap<String, Registered>,
    closed: bool,
    limit: usize,
    shutdown_timeouts: Counter,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
            closed: false,
            limit: MAX_WRITERS,
            shutdown_timeouts: Counter::new(),
        }
    }
}

impl Registry {
    fn reap(&mut self) {
        self.open
            .retain(|_, entry| entry.sink.strong_count() != 0 || !entry.worker.finished());
    }

    fn existing(&self, name: &str) -> Result<Option<Arc<Sink>>, String> {
        if self.closed {
            return Err("log writers are shutting down".into());
        }
        let Some(entry) = self.open.get(&identity(name)) else {
            return Ok(None);
        };
        if let Some(sink) = entry.sink.upgrade() {
            return Ok(Some(sink));
        }
        Err(format!("log writer `{name}` is still draining"))
    }
}

#[derive(Debug, Default, Clone)]
pub struct Sinks {
    open: Arc<Mutex<Registry>>,
}

impl Sinks {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stage(&self) -> StagedSinks {
        StagedSinks::new(self.clone())
    }
    pub fn stderr(&self) -> Result<Arc<Sink>, String> {
        self.resolve("stderr", |name| spawn(name, Destination::Stderr, Rotate::NEVER))
    }
    /// Immediate construction/policy update for standalone callers. Hosts use a
    /// candidate's StagedSinks so existing writers change only at publication.
    pub fn file(&self, path: &Path, rotate: Rotate) -> Result<Arc<Sink>, String> {
        let sink = self.file_handle(path, rotate)?;
        *sink.policy.lock().unwrap_or_else(|p| p.into_inner()) = rotate;
        Ok(sink)
    }
    pub(crate) fn file_handle(&self, path: &Path, rotate: Rotate) -> Result<Arc<Sink>, String> {
        if self.open.lock().unwrap_or_else(|p| p.into_inner()).closed {
            return Err("log writers are shutting down".into());
        }
        // The file can temporarily be absent while its existing writer rolls
        // it. Resolve the stable parent first, before opening another handle.
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && let Ok(parent) = std::fs::canonicalize(parent)
            && let Some(existing) = self.get(&label(&parent.join(name)))?
        {
            return Ok(existing);
        }
        let file = open(path)?;
        let written = file
            .metadata()
            .map_err(|error| format!("cannot inspect log file: {error}"))?
            .len();
        let path =
            std::fs::canonicalize(path).map_err(|error| format!("cannot resolve log file: {error}"))?;
        let name = label(&path);
        self.resolve(&name, move |name| {
            spawn(
                name,
                Destination::File {
                    file: Some(file),
                    path,
                    written,
                    kept: rotate.keep,
                },
                rotate,
            )
        })
    }
    fn resolve(&self, name: &str, create: impl FnOnce(&str) -> Sink) -> Result<Arc<Sink>, String> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.reap();
        let key = identity(name);
        if let Some(existing) = open.existing(name)? {
            return Ok(existing);
        }
        if open.open.len() >= open.limit {
            return Err(format!(
                "log writer limit ({}) reached, including retiring writers",
                open.limit
            ));
        }
        let sink = Arc::new(create(name));
        open.open.insert(
            key,
            Registered {
                sink: Arc::downgrade(&sink),
                io: Arc::downgrade(&sink.io),
                worker: sink.worker.clone(),
            },
        );
        Ok(sink)
    }
    fn get(&self, name: &str) -> Result<Option<Arc<Sink>>, String> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.reap();
        open.existing(name)
    }
    pub fn snapshot(&self) -> Vec<Arc<Sink>> {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.reap();
        let mut sinks: Vec<_> = open
            .open
            .values()
            .filter_map(|entry| entry.sink.upgrade())
            .collect();
        sinks.sort_by(|left, right| left.name.cmp(&right.name));
        sinks
    }

    /// Close even writers still referenced by a serving runtime, then wait
    /// asynchronously for queue drain, final flush and destination close.
    /// The host must impose a deadline; timeout cannot interrupt OS disk I/O.
    pub fn shutdown(&self) -> Drain {
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.closed = true;
        open.reap();
        let workers: Vec<_> = open.open.values().map(|entry| entry.worker.clone()).collect();
        for worker in &workers {
            worker.close();
        }
        Drain::new(workers)
    }

    /// Includes closed queues whose OS writer or final flush is still pending.
    pub fn pending_writers(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .open
            .values()
            .filter(|entry| !entry.worker.finished())
            .count()
    }

    /// The host calls this when its drain deadline expires. No diagnostic I/O
    /// runs on that shutdown path, including when stderr itself is stalled.
    pub fn record_shutdown_timeout(&self) {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_timeouts
            .increment();
    }

    pub fn shutdown_timeouts(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_timeouts
            .get()
    }
}

impl crate::health::LogStatus for Sinks {
    fn failures(&self) -> Vec<String> {
        // Health samples only lightweight atomic status ownership.
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let mut failures: Vec<_> = open
            .open
            .iter()
            .filter(|(_, entry)| {
                entry.sink.strong_count() != 0 && entry.io.upgrade().is_some_and(|io| io.failed())
            })
            .map(|(name, _)| name.clone())
            .collect();
        failures.sort();
        failures
    }
}

enum Destination {
    Stderr,
    File {
        file: Option<File>,
        path: PathBuf,
        written: u64,
        kept: usize,
    },
    #[cfg(test)]
    Injected {
        writer: Box<dyn Write + Send>,
        rotation_error: bool,
    },
}

impl Destination {
    fn write(&mut self, line: &[u8], rotate: Rotate) -> Result<(), (Operation, io::Error)> {
        match self {
            Self::Stderr => std::io::stderr()
                .write_all(line)
                .map_err(|error| (Operation::Write, error)),
            Self::File {
                file,
                path,
                written,
                kept,
            } => {
                if file.is_none() {
                    let reopened = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&*path)
                        .map_err(|error| (Operation::Write, error))?;
                    *written = reopened
                        .metadata()
                        .map_err(|error| (Operation::Write, error))?
                        .len();
                    *file = Some(reopened);
                }
                file.as_mut()
                    .unwrap()
                    .write_all(line)
                    .map_err(|error| (Operation::Write, error))?;
                *written = written.saturating_add(line.len() as u64);
                if rotate.size > 0 && *written >= rotate.size {
                    file.take();
                    *file = Some(roll(path, rotate, *kept).map_err(|error| (Operation::Rotate, error))?);
                    *written = 0;
                    *kept = rotate.keep;
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Injected {
                writer,
                rotation_error,
            } => {
                writer
                    .write_all(line)
                    .map_err(|error| (Operation::Write, error))?;
                if *rotation_error {
                    return Err((Operation::Rotate, io::Error::other("injected rotation failure")));
                }
                Ok(())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stderr => std::io::stderr().flush(),
            Self::File { file: Some(file), .. } => file.flush(),
            Self::File { file: None, .. } => Ok(()),
            #[cfg(test)]
            Self::Injected { writer, .. } => writer.flush(),
        }
    }
}

fn label(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}
fn identity(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
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
fn missing_is_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
fn roll(path: &Path, rotate: Rotate, previous_keep: usize) -> io::Result<File> {
    let numbered = |index: usize| {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    };
    if rotate.keep < previous_keep {
        for index in (rotate.keep + 1)..=previous_keep {
            missing_is_ok(std::fs::remove_file(numbered(index)))?;
        }
    }
    if rotate.keep > 0 {
        missing_is_ok(std::fs::remove_file(numbered(rotate.keep)))?;
        for index in (1..rotate.keep).rev() {
            missing_is_ok(std::fs::rename(numbered(index), numbered(index + 1)))?;
        }
        std::fs::rename(path, numbered(1))?;
    } else {
        missing_is_ok(std::fs::remove_file(path))?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
#[path = "sink_policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
#[path = "sink_lifecycle_tests.rs"]
mod lifecycle_tests;

fn spawn(name: &str, destination: Destination, initial: Rotate) -> Sink {
    let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE);
    let queue = Arc::new(Mutex::new(Some(sender)));
    let completion = Arc::new(Completion::default());
    let finished = completion.clone();
    let label = name.to_owned();
    let policy = Arc::new(Mutex::new(initial));
    let worker_policy = policy.clone();
    let io = Arc::new(IoStats::default());
    let worker_io = io.clone();
    let worker = std::thread::Builder::new()
        .name(format!("gatel-log-{label}"))
        .spawn(move || {
            // Locals drop in reverse order, including during panic: closing
            // the destination precedes publishing completion to async waiters.
            let _completed = Completed(finished);
            let mut destination = destination;
            let mut last_report = None;
            loop {
                let (operation, result, stop) = match receiver.recv_timeout(FLUSH_AFTER) {
                    Ok(line) => {
                        let rotate = *worker_policy.lock().unwrap_or_else(|p| p.into_inner());
                        // The policy lock is never held across disk I/O.
                        let result = destination.write(&line, rotate);
                        match result {
                            Ok(()) => {
                                worker_io.success(Operation::Write);
                                worker_io.success(Operation::Rotate);
                                continue;
                            }
                            Err((Operation::Rotate, error)) => {
                                worker_io.success(Operation::Write);
                                (Operation::Rotate, Err(error), false)
                            }
                            Err((operation, error)) => (operation, Err(error), false),
                        }
                    }
                    Err(error) => (
                        Operation::Flush,
                        destination.flush(),
                        matches!(error, std::sync::mpsc::RecvTimeoutError::Disconnected),
                    ),
                };
                match result {
                    Ok(()) => worker_io.success(operation),
                    Err(error) => {
                        worker_io.error(operation);
                        if last_report.is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(30)) {
                            // Independent diagnostic stream, never enqueue into the
                            // failed sink and never panic if stderr is also broken.
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "gatel log {operation:?} failed for {label}: {error}"
                            );
                            last_report = Some(Instant::now());
                        }
                    }
                }
                if stop {
                    return;
                }
            }
        })
        .expect("a log writer thread");
    Sink {
        name: name.to_owned(),
        worker: Arc::new(Worker::new(worker, completion, queue.clone())),
        queue,
        dropped: Counter::new(),
        policy,
        io,
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
        assert!(eventually(|| sinks.pending_writers() == 0));
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
