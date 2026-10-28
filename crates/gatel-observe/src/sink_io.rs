//! Separate queue loss, file I/O failures and recovery; never log via the sink.
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::instrument::Counter;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Operation {
    Write = 1,
    Flush = 2,
    Rotate = 4,
}

#[derive(Debug, Default)]
pub(crate) struct IoStats {
    pub write: Counter,
    pub flush: Counter,
    pub rotate: Counter,
    last_write: AtomicU64,
    failed: AtomicU8,
}

impl IoStats {
    pub fn error(&self, operation: Operation) {
        match operation {
            Operation::Write => &self.write,
            Operation::Flush => &self.flush,
            Operation::Rotate => &self.rotate,
        }
        .increment();
        self.failed.fetch_or(operation as u8, Ordering::Relaxed);
    }
    pub fn success(&self, operation: Operation) {
        self.failed.fetch_and(!(operation as u8), Ordering::Relaxed);
        if matches!(operation, Operation::Write) {
            self.last_write.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Ordering::Relaxed,
            );
        }
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed) != 0
    }
    pub fn last_write(&self) -> u64 {
        self.last_write.load(Ordering::Relaxed)
    }
}
