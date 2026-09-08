//! Admission for trusted plugins' blocking work, not an allocator or sandbox.
//! Reservations cover queued and running closures, including cancelled waiters.
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub jobs: usize,
    pub memory_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            jobs: 16,
            memory_bytes: 256 * 1024 * 1024,
        }
    }
}
impl Limits {
    fn validate(self) -> Result<Self, &'static str> {
        if !(1..=1024).contains(&self.jobs) || self.memory_bytes == 0 {
            return Err("blocking limits require 1..1024 jobs and nonzero memory bytes");
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub limits: Limits,
    pub jobs: usize,
    pub memory_bytes: u64,
    pub closed: bool,
    pub running: usize,
    pub counters: Counters,
}

/// Fixed-cardinality, saturating lifetime counters. No request-derived labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub admitted: u64,
    pub rejected_jobs: u64,
    pub rejected_memory: u64,
    pub rejected_closed: u64,
    pub completed: u64,
    pub panicked: u64,
    pub abandoned: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Saturated,
    Closed,
    ExecutionFailed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Saturated => "blocking work budget exhausted",
            Self::Closed => "blocking work budget closed",
            Self::ExecutionFailed => "blocking work execution failed",
        })
    }
}
impl std::error::Error for Error {}

/// Host-only ownership: do not hand this controller to a directive. Dropping
/// the controller does not close live executors; use `close` explicitly.
pub struct Budget(Arc<Mutex<Snapshot>>);
impl Budget {
    pub fn new(limits: Limits) -> Result<Self, &'static str> {
        Ok(Self(Arc::new(Mutex::new(Snapshot {
            limits: limits.validate()?,
            jobs: 0,
            memory_bytes: 0,
            closed: false,
            running: 0,
            counters: Counters::default(),
        }))))
    }

    pub fn executor(&self) -> Executor {
        Executor(self.0.clone())
    }

    /// Atomic replacement retains actual usage. Invalid limits change nothing.
    /// Shrinking below usage rejects new work until enough reservations finish.
    /// Reconfiguration never reopens a closed budget.
    pub fn reconfigure(&self, limits: Limits) -> Result<(), &'static str> {
        let limits = limits.validate()?;
        self.0.lock().unwrap_or_else(|p| p.into_inner()).limits = limits;
        Ok(())
    }

    pub fn close(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
    }

    /// Pure preparation: verify ownership/limits without changing live usage.
    pub fn prepare(&self, executor: &Executor, limits: Limits) -> Result<PreparedChange, &'static str> {
        if !Arc::ptr_eq(&self.0, &executor.0) {
            return Err("blocking executor does not belong to this budget");
        }
        Ok(PreparedChange {
            state: self.0.clone(),
            limits: limits.validate()?,
        })
    }
}

/// Inert until the host enters its validated publication critical section.
#[derive(Debug)]
pub struct PreparedChange {
    state: Arc<Mutex<Snapshot>>,
    limits: Limits,
}
impl PreparedChange {
    pub fn begin_publication(&self) -> Publication<'_> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let previous = state.limits;
        state.limits = self.limits;
        Publication {
            state,
            previous,
            published: false,
        }
    }
}
pub struct Publication<'a> {
    state: std::sync::MutexGuard<'a, Snapshot>,
    previous: Limits,
    published: bool,
}
impl Publication<'_> {
    pub fn publish(mut self) {
        self.published = true;
    }
}
impl Drop for Publication<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.state.limits = self.previous;
        }
    }
}

/// Cloneable execution capability without permission to change limits.
#[derive(Clone)]
pub struct Executor(Arc<Mutex<Snapshot>>);
impl Executor {
    /// Default host-wide budget, shared across all server instances/generations.
    /// Embedded hosts may instead inject a separately owned Budget's executor.
    pub fn process_default() -> Self {
        static DEFAULT: OnceLock<Budget> = OnceLock::new();
        DEFAULT
            .get_or_init(|| Budget::new(Limits::default()).expect("valid built-in blocking budget"))
            .executor()
    }

    pub fn snapshot(&self) -> Snapshot {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn metrics(&self) -> gatel_observe::blocking::BlockingSample {
        let state = self.snapshot();
        let counts = state.counters;
        gatel_observe::blocking::BlockingSample {
            jobs: state.jobs as u64,
            running: state.running as u64,
            memory_bytes: state.memory_bytes,
            jobs_limit: state.limits.jobs as u64,
            memory_limit_bytes: state.limits.memory_bytes,
            closed: state.closed,
            admitted: counts.admitted,
            rejected_jobs: counts.rejected_jobs,
            rejected_memory: counts.rejected_memory,
            rejected_closed: counts.rejected_closed,
            completed: counts.completed,
            panicked: counts.panicked,
            abandoned: counts.abandoned,
        }
    }

    fn reserve(&self, memory_bytes: u64) -> Result<Reservation, Error> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            state.counters.rejected_closed = state.counters.rejected_closed.saturating_add(1);
            return Err(Error::Closed);
        }
        // Exclusive reasons, in stable precedence: closed, jobs, then memory.
        if state.jobs >= state.limits.jobs {
            state.counters.rejected_jobs = state.counters.rejected_jobs.saturating_add(1);
            return Err(Error::Saturated);
        }
        let memory = state.memory_bytes.checked_add(memory_bytes);
        if memory.is_none_or(|memory| memory > state.limits.memory_bytes) {
            state.counters.rejected_memory = state.counters.rejected_memory.saturating_add(1);
            return Err(Error::Saturated);
        }
        state.jobs += 1;
        state.memory_bytes = memory.expect("checked memory sum");
        state.counters.admitted = state.counters.admitted.saturating_add(1);
        Ok(Reservation {
            executor: self.clone(),
            memory_bytes,
            phase: Phase::Queued,
        })
    }

    /// No admission queue. `memory_bytes` is a trusted algorithm's declared
    /// working memory, not an enforced RSS bound. Never run async work here.
    /// Dropping the waiter does not stop an already queued/running closure.
    /// Must be polled inside a Tokio runtime; not a wall-clock work deadline.
    pub async fn run<T: Send + 'static>(
        &self,
        memory_bytes: u64,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Error> {
        let mut reservation = self.reserve(memory_bytes)?;
        tokio::task::spawn_blocking(move || {
            reservation.start();
            let result = work();
            reservation.phase = Phase::Completed;
            drop(reservation);
            result
        })
        .await
        .map_err(|_| Error::ExecutionFailed)
    }
}

enum Phase {
    Queued,
    Running,
    Completed,
}
struct Reservation {
    executor: Executor,
    memory_bytes: u64,
    phase: Phase,
}
impl Reservation {
    fn start(&mut self) {
        self.executor.0.lock().unwrap_or_else(|p| p.into_inner()).running += 1;
        self.phase = Phase::Running;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.executor.0.lock().unwrap_or_else(|p| p.into_inner());
        state.jobs -= 1;
        state.memory_bytes -= self.memory_bytes;
        let counter = match self.phase {
            Phase::Queued => &mut state.counters.abandoned,
            Phase::Running => {
                state.running -= 1;
                &mut state.counters.panicked
            }
            Phase::Completed => {
                state.running -= 1;
                &mut state.counters.completed
            }
        };
        *counter = counter.saturating_add(1);
    }
}

#[cfg(test)]
#[path = "blocking_tests.rs"]
mod tests;
