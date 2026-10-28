use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;

fn directory(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gatel-log-policy-{}-{}-{tag}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn eventually(mut condition: impl FnMut() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("writer did not reach the expected state");
}
fn policy(sinks: &Sinks, path: &Path, rotate: Rotate) -> Arc<Sink> {
    let staged = sinks.stage();
    let sink = staged.file(path, rotate).unwrap();
    staged.finish().begin().publish();
    sink
}

#[test]
fn only_publication_changes_rotation_and_conflicting_aliases_are_refused() {
    let directory = directory("transaction");
    let path = directory.join("access.log");
    let sinks = Sinks::new();
    let sink = sinks.file(&path, Rotate::NEVER).unwrap();
    let candidate = sinks.stage();
    assert!(Arc::ptr_eq(
        &sink,
        &candidate.file(&path, Rotate { size: 8, keep: 2 }).unwrap()
    ));
    assert!(
        candidate
            .file(&directory.join("./access.log"), Rotate::NEVER)
            .is_err()
    );
    let failed = candidate.finish();
    drop(failed.begin());
    drop(failed);
    sink.write("before publication");
    eventually(|| {
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("before publication")
    });
    assert!(!path.with_extension("log.1").exists());
    let reused = policy(&sinks, &path, Rotate { size: 8, keep: 2 });
    assert!(Arc::ptr_eq(&sink, &reused));
    sink.write("first rotation");
    eventually(|| path.with_extension("log.1").exists());
    sink.write("second rotation");
    eventually(|| path.with_extension("log.2").exists());
    policy(&sinks, &path, Rotate { size: 8, keep: 1 });
    sink.write("smaller retention");
    eventually(|| {
        std::fs::read_to_string(path.with_extension("log.1"))
            .unwrap_or_default()
            .contains("smaller retention")
    });
    assert!(!path.with_extension("log.2").exists());
    policy(&sinks, &path, Rotate::NEVER);
    sink.write("rotation disabled");
    eventually(|| {
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("rotation disabled")
    });
    policy(&sinks, &path, Rotate { size: 1024, keep: 1 });
    sink.write("larger threshold");
    eventually(|| {
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("larger threshold")
    });
    assert!(
        !std::fs::read_to_string(path.with_extension("log.1"))
            .unwrap()
            .contains("larger threshold")
    );
    let direct = sinks.file(&path, Rotate { size: 8, keep: 1 }).unwrap();
    assert!(Arc::ptr_eq(&sink, &direct));
    direct.write("standalone policy update");
    eventually(|| {
        std::fs::read_to_string(path.with_extension("log.1"))
            .unwrap_or_default()
            .contains("standalone policy update")
    });
}

struct Faults {
    write: Arc<AtomicBool>,
    flush: Arc<AtomicBool>,
}
impl Write for Faults {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.write.load(Ordering::Relaxed) {
            Err(io::Error::other("injected ENOSPC"))
        } else {
            Ok(data.len())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.flush.load(Ordering::Relaxed) {
            Err(io::Error::other("injected EIO"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn write_flush_failures_are_distinct_visible_and_recover_without_queue_loss() {
    let telemetry = crate::Telemetry::new();
    telemetry.health.committed(1);
    let write = Arc::new(AtomicBool::new(true));
    let flush = Arc::new(AtomicBool::new(true));
    let sink = telemetry.sinks.resolve("injected", |name| {
        spawn(
            name,
            Destination::Injected {
                writer: Box::new(Faults {
                    write: write.clone(),
                    flush: flush.clone(),
                }),
                rotation_error: false,
            },
            Rotate::NEVER,
        )
    });
    sink.write("must not block on a failing disk");
    eventually(|| sink.write_errors() == 1 && sink.flush_errors() > 0);
    assert_eq!(sink.dropped(), 0);
    assert_eq!(sink.last_success(), 0);
    let report = telemetry.health.report();
    assert_eq!(report.state, crate::State::Degraded);
    assert!(
        report
            .reasons
            .iter()
            .any(|reason| reason.code() == "log_io_failed")
    );
    let metrics = crate::prometheus(&telemetry);
    assert!(metrics.contains("gatel_log_write_errors_total{sink=\"injected\"} 1"));
    assert!(metrics.contains("gatel_log_flush_errors_total{sink=\"injected\"}"));
    assert!(metrics.contains("gatel_log_io_failed{sink=\"injected\"} 1"));
    assert!(metrics.contains("gatel_health_reason{reason=\"log_io_failed\"} 1"));
    write.store(false, Ordering::Relaxed);
    flush.store(false, Ordering::Relaxed);
    sink.write("recovered");
    eventually(|| !sink.io_failed() && sink.last_success() > 0);
    assert_eq!(telemetry.health.report().state, crate::State::Ready);
    assert_eq!(sink.write_errors(), 1);
}

#[test]
fn rotation_filesystem_errors_are_counted_and_writer_can_recover() {
    let directory = directory("rotation-error");
    let path = directory.join("access.log");
    std::fs::create_dir(path.with_extension("log.1")).unwrap();
    let sink = Sinks::new().file(&path, Rotate { size: 8, keep: 1 }).unwrap();
    sink.write("will trigger a rotation");
    eventually(|| sink.rotation_errors() == 1);
    assert_eq!(sink.write_errors(), 0);
    assert_eq!(sink.dropped(), 0);
    assert!(sink.io_failed());
    std::fs::remove_dir(path.with_extension("log.1")).unwrap();
    sink.write("recover after storage repair");
    eventually(|| !sink.io_failed() && path.with_extension("log.1").is_file());
}
