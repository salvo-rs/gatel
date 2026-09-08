//! Generation-owned tasks: prepare is inert, activation is idempotent, and
//! final generation release cancels its work instead of detaching it.
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use gatel_plan::{BackgroundTask, Shutdown, ShutdownSignal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskOutcome {
    Pending,
    Running,
    Completed,
    Failed,
    Panicked,
    Cancelled,
}

#[derive(Debug)]
struct Report {
    name: &'static str,
    outcome: AtomicU8,
}

struct Completion(Arc<Report>);
impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.0.outcome.compare_exchange(
            TaskOutcome::Running as u8,
            TaskOutcome::Cancelled as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

#[derive(Debug)]
enum State {
    Pending(Vec<BackgroundTask>),
    Running(Vec<RunningTask>),
    Stopping(Vec<RunningTask>),
    Stopped,
}

#[derive(Debug)]
struct RunningTask {
    handle: Option<tokio::task::JoinHandle<()>>,
    cooperative: bool,
}
impl RunningTask {
    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

/// If the caller cancels shutdown, retain the join handles for the next call.
/// Never detach cleanup merely because the shutdown future was dropped.
struct ShutdownWork<'a> {
    scope: &'a TaskScope,
    tasks: Option<Vec<RunningTask>>,
}
impl Drop for ShutdownWork<'_> {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.take() {
            for task in &tasks {
                task.abort();
            }
            *self.scope.state.lock().unwrap() = State::Stopping(tasks);
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskScope {
    state: Mutex<State>,
    reports: Vec<Arc<Report>>,
    signal: ShutdownSignal,
    shutdown: Shutdown,
    shutdown_lock: tokio::sync::Mutex<()>,
}

impl TaskScope {
    pub const LIMIT: usize = 64;
    pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new(tasks: Vec<BackgroundTask>) -> Result<Self, String> {
        if tasks.len() > Self::LIMIT {
            return Err(format!(
                "generation exceeds {} plugin background tasks",
                Self::LIMIT
            ));
        }
        let (signal, shutdown) = ShutdownSignal::new();
        Ok(Self {
            signal,
            shutdown,
            shutdown_lock: tokio::sync::Mutex::new(()),
            reports: tasks
                .iter()
                .map(|task| {
                    Arc::new(Report {
                        name: task.name,
                        outcome: AtomicU8::new(TaskOutcome::Pending as u8),
                    })
                })
                .collect(),
            state: Mutex::new(State::Pending(tasks)),
        })
    }

    /// Requires a Tokio runtime; called only on a published generation.
    pub fn activate(&self) {
        let executor = tokio::runtime::Handle::current();
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, State::Pending(_)) {
            return;
        }
        let State::Pending(tasks) = std::mem::replace(&mut *state, State::Stopped) else {
            unreachable!()
        };
        *state = State::Running(
            tasks
                .into_iter()
                .zip(&self.reports)
                .map(|(task, report)| {
                    report.outcome.store(TaskOutcome::Running as u8, Ordering::SeqCst);
                    let completion = Completion(report.clone());
                    let shutdown = self.shutdown.clone();
                    let cooperative = task.cooperative;
                    // Execute the factory inside Tokio, never under this mutex.
                    let handle = executor.spawn(async move {
                        let outcome = match std::panic::AssertUnwindSafe(async { (task.run)(shutdown).await })
                            .catch_unwind()
                            .await
                        {
                            Ok(Ok(())) => TaskOutcome::Completed,
                            Ok(Err(_)) => TaskOutcome::Failed,
                            Err(_) => TaskOutcome::Panicked,
                        };
                        // Do not publish arbitrary error/panic text: it may contain secrets.
                        completion.0.outcome.store(outcome as u8, Ordering::SeqCst);
                    });
                    RunningTask {
                        handle: Some(handle),
                        cooperative,
                    }
                })
                .collect(),
        );
    }

    /// Stable declaration order; names can repeat across different plugin owners.
    pub fn outcomes(&self) -> Vec<(&'static str, TaskOutcome)> {
        self.reports
            .iter()
            .map(|report| {
                let outcome = match report.outcome.load(Ordering::SeqCst) {
                    0 => TaskOutcome::Pending,
                    1 => TaskOutcome::Running,
                    2 => TaskOutcome::Completed,
                    3 => TaskOutcome::Failed,
                    4 => TaskOutcome::Panicked,
                    _ => TaskOutcome::Cancelled,
                };
                (report.name, outcome)
            })
            .collect()
    }

    /// Signal every task first, then allow cooperative work one shared grace
    /// period. Abort and join the remainder. Concurrent callers wait for the
    /// same cleanup; a cancelled caller leaves handles available for retry.
    /// Synchronously blocking plugins cannot be forcibly interrupted by Tokio.
    pub async fn shutdown(&self) {
        let _lock = self.shutdown_lock.lock().await;
        self.signal.request();
        let deadline = tokio::time::Instant::now() + Self::SHUTDOWN_GRACE;
        let previous = std::mem::replace(&mut *self.state.lock().unwrap(), State::Stopped);
        for report in &self.reports {
            let _ = report.outcome.compare_exchange(
                TaskOutcome::Pending as u8,
                TaskOutcome::Cancelled as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        if let State::Running(tasks) | State::Stopping(tasks) = previous {
            let mut work = ShutdownWork {
                scope: self,
                tasks: Some(tasks),
            };
            let tasks = work.tasks.as_mut().unwrap();
            for task in tasks.iter() {
                if !task.cooperative {
                    task.abort();
                }
            }
            for task in tasks.iter_mut() {
                if let Some(handle) = &mut task.handle {
                    if tokio::time::timeout_at(deadline, handle).await.is_err() {
                        break;
                    }
                    task.handle = None;
                }
            }
            for task in tasks.iter() {
                task.abort();
            }
            for task in tasks.iter_mut() {
                if let Some(handle) = &mut task.handle {
                    let _ = handle.await;
                    task.handle = None;
                }
            }
            work.tasks = None;
        }
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        self.signal.request();
        if let State::Running(tasks) | State::Stopping(tasks) = self.state.get_mut().unwrap() {
            for task in tasks {
                task.abort();
            }
        }
    }
}

impl TaskScope {
    pub(crate) fn pending(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Pending(_))
    }
    pub(crate) fn stopped(&self) -> bool {
        // State is temporarily Stopped while shutdown owns join handles.
        let Ok(_shutdown) = self.shutdown_lock.try_lock() else {
            return false;
        };
        matches!(*self.state.lock().unwrap(), State::Stopped)
    }
    pub(crate) fn abort(&self) {
        self.signal.request();
        if let State::Running(tasks) | State::Stopping(tasks) = &*self.state.lock().unwrap() {
            for task in tasks {
                task.abort();
            }
        }
    }
}

/// A generation's ownership token. The host retains only its task scope while
/// retiring, never the runtime's routes/listeners or the committed generation.
#[derive(Debug)]
pub struct PluginTasks {
    inner: Arc<TaskScope>,
    retirement: Option<crate::plugin_retirement::Ticket>,
}
impl PluginTasks {
    pub const LIMIT: usize = TaskScope::LIMIT;
    pub const SHUTDOWN_GRACE: std::time::Duration = TaskScope::SHUTDOWN_GRACE;
    /// Standalone scope: the caller must explicitly await shutdown; drop aborts.
    pub fn new(tasks: Vec<BackgroundTask>) -> Result<Self, String> {
        Ok(Self {
            inner: Arc::new(TaskScope::new(tasks)?),
            retirement: None,
        })
    }
    pub fn managed(
        tasks: Vec<BackgroundTask>,
        owner: &Arc<crate::plugin_retirement::Retirements>,
    ) -> Result<Self, String> {
        let empty = tasks.is_empty();
        let mut scope = Self::new(tasks)?;
        if !empty {
            scope.retirement = Some(owner.register(scope.inner.clone())?);
        }
        Ok(scope)
    }
    pub fn activate(&self) {
        self.inner.activate();
    }
    pub fn outcomes(&self) -> Vec<(&'static str, TaskOutcome)> {
        self.inner.outcomes()
    }
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}
impl Drop for PluginTasks {
    fn drop(&mut self) {
        // Ticket drop submits cleanup, while self.inner is still alive.
        drop(self.retirement.take());
    }
}

#[cfg(test)]
#[path = "plugin_shutdown_tests.rs"]
mod shutdown_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct OnDrop(Arc<AtomicUsize>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn task_budget_and_shutdown_before_activation_are_enforced() {
        let tasks = (0..=PluginTasks::LIMIT)
            .map(|_| BackgroundTask::new("unused", || async {}))
            .collect();
        assert!(PluginTasks::new(tasks).is_err());
        let scope = PluginTasks::new(vec![BackgroundTask::new("unused", || async {
            panic!("shutdown scope was reactivated");
        })])
        .unwrap();
        scope.shutdown().await;
        scope.activate();
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn prepare_is_inert_activation_once_and_shutdown_joins_cancellation() {
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (ready, wait) = tokio::sync::oneshot::channel();
        let task = {
            let started = started.clone();
            let dropped = dropped.clone();
            BackgroundTask::new("test", move || {
                started.fetch_add(1, Ordering::SeqCst);
                async move {
                    let _guard = OnDrop(dropped);
                    let _ = ready.send(());
                    std::future::pending::<()>().await;
                }
            })
        };
        let scope = PluginTasks::new(vec![task]).unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 0);
        scope.activate();
        scope.activate();
        wait.await.unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1);
        scope.shutdown().await;
        assert_eq!(scope.outcomes(), vec![("test", TaskOutcome::Cancelled)]);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        scope.activate();
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn completion_errors_and_factory_panics_have_distinct_outcomes() {
        let scope = PluginTasks::new(vec![
            BackgroundTask::new("success", || async {}),
            BackgroundTask::fallible("error", || async { Err("private material".into()) }),
            BackgroundTask::new("panic", || {
                panic!("test factory panic");
                #[allow(unreachable_code)]
                async {}
            }),
        ])
        .unwrap();
        scope.activate();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while scope
                .outcomes()
                .iter()
                .any(|(_, status)| *status == TaskOutcome::Running)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            scope.outcomes(),
            vec![
                ("success", TaskOutcome::Completed),
                ("error", TaskOutcome::Failed),
                ("panic", TaskOutcome::Panicked),
            ]
        );
        scope.shutdown().await;
        assert_eq!(scope.outcomes()[1].1, TaskOutcome::Failed);
    }

    #[tokio::test]
    async fn discarded_candidate_never_invokes_factory() {
        let scope = PluginTasks::new(vec![BackgroundTask::new("must-not-run", || {
            panic!("candidate started work");
            #[allow(unreachable_code)]
            async {}
        })])
        .unwrap();
        drop(scope);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn final_owner_release_cancels_running_work() {
        let (ready, wait) = tokio::sync::oneshot::channel();
        let (dropped, mut observed) = tokio::sync::mpsc::channel::<()>(1);
        let scope = Arc::new(
            PluginTasks::new(vec![BackgroundTask::new("test", move || async move {
                let _held = dropped;
                let _ = ready.send(());
                std::future::pending::<()>().await;
            })])
            .unwrap(),
        );
        let old_connection = scope.clone();
        scope.activate();
        wait.await.unwrap();
        drop(scope);
        assert!(matches!(
            observed.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(old_connection);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
                .await
                .unwrap(),
            None
        );
    }
}
