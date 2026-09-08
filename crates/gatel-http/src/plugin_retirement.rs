//! Process-owned, bounded task retirement. Capacity is reserved during prepare,
//! before a generation may publish work that will later need async cleanup.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use crate::plugin_tasks::TaskScope;

#[derive(Debug)]
struct Entry {
    scope: Arc<TaskScope>,
    executor: tokio::runtime::Handle,
    job: Option<Arc<Job>>,
}
#[derive(Debug)]
struct Job {
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    abort: tokio::task::AbortHandle,
}
impl Job {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            abort: handle.abort_handle(),
            handle: tokio::sync::Mutex::new(Some(handle)),
        }
    }
    fn is_finished(&self) -> bool {
        self.handle
            .try_lock()
            .is_ok_and(|handle| handle.as_ref().is_none_or(|handle| handle.is_finished()))
    }
    async fn join(&self) {
        let mut handle = self.handle.lock().await;
        if let Some(running) = handle.as_mut() {
            let _ = running.await;
        }
        *handle = None;
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
#[derive(Debug, Default)]
struct State {
    closed: bool,
    next: u64,
    entries: BTreeMap<u64, Entry>,
}
impl State {
    fn reap(&mut self) {
        self.entries.retain(|_, entry| {
            !(entry.job.as_ref().is_some_and(|job| job.is_finished()) && entry.scope.stopped())
        });
    }
}

#[derive(Debug, Default)]
pub struct Retirements {
    state: Mutex<State>,
    shutdown_lock: tokio::sync::Mutex<()>,
}
impl Retirements {
    /// Includes prepared, active, connection-pinned and retiring generations.
    pub const LIMIT: usize = 64;

    pub(crate) fn register(self: &Arc<Self>, scope: Arc<TaskScope>) -> Result<Ticket, String> {
        let executor = tokio::runtime::Handle::try_current()
            .map_err(|_| "plugin work preparation requires an async runtime")?;
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err("plugin retirement scope is shut down".into());
        }
        state.reap();
        if state.entries.len() >= Self::LIMIT {
            return Err("plugin generation retirement capacity exhausted".into());
        }
        let id = state.next;
        state.next = id.checked_add(1).ok_or("plugin retirement identity exhausted")?;
        state.entries.insert(
            id,
            Entry {
                scope,
                executor,
                job: None,
            },
        );
        Ok(Ticket {
            owner: self.clone(),
            id,
        })
    }

    fn retire(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.get(&id) else {
            return;
        };
        if entry.scope.pending() || entry.scope.stopped() {
            // Discarded preparation remains inert, and completed scopes need no job.
            state.entries.remove(&id);
            return;
        }
        if state.closed {
            // Shutdown already owns this registered scope, including live owners.
            return;
        }
        let entry = state.entries.get_mut(&id).unwrap();
        let scope = entry.scope.clone();
        entry.job = Some(Arc::new(Job::new(
            entry.executor.spawn(async move { scope.shutdown().await }),
        )));
    }

    pub fn tracked_generations(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        state.reap();
        state.entries.len()
    }

    /// Seal registration and join every registered scope, including a generation
    /// still pinned by an external owner. Cancelled callers can retry: scopes and
    /// retirement jobs remain registered until this operation actually finishes.
    pub async fn shutdown(&self) {
        let _lock = self.shutdown_lock.lock().await;
        let scopes = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state
                .entries
                .values()
                .map(|entry| entry.scope.clone())
                .collect::<Vec<_>>()
        };
        let mut cleanup = scopes
            .iter()
            .map(|scope| scope.shutdown())
            .collect::<FuturesUnordered<_>>();
        while cleanup.next().await.is_some() {}
        // Join handles stay owned by the registry across cancellation of this
        // await; each job serializes concurrent/retried joins independently.
        let jobs = self
            .state
            .lock()
            .unwrap()
            .entries
            .values()
            .filter_map(|entry| entry.job.clone())
            .collect::<Vec<_>>();
        let mut joins = jobs.iter().map(|job| job.join()).collect::<FuturesUnordered<_>>();
        while joins.next().await.is_some() {}
        self.state.lock().unwrap().entries.clear();
    }
}
impl Drop for Retirements {
    fn drop(&mut self) {
        // Emergency fallback only. Normal server shutdown awaits cleanup above.
        for entry in self.state.get_mut().unwrap().entries.values() {
            entry.scope.abort();
            if let Some(job) = &entry.job {
                job.abort.abort();
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Ticket {
    owner: Arc<Retirements>,
    id: u64,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.owner.retire(self.id);
    }
}

#[cfg(test)]
#[path = "plugin_retirement_tests.rs"]
mod tests;
