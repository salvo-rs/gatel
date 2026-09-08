//! Declarative, generation-scoped work. This module never spawns a task.
use std::future::Future;
use std::pin::Pin;

/// A level-triggered shutdown request, including cancellation before activation.
/// It grants time for cleanup, not permission to detach work from its generation.
#[derive(Debug, Clone)]
pub struct Shutdown(tokio::sync::watch::Receiver<bool>);
impl Shutdown {
    pub fn is_requested(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }
    pub async fn requested(&mut self) {
        while !self.is_requested() {
            if self.0.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Host-owned signal. Dropping its last owner also requests shutdown.
#[derive(Debug)]
pub struct ShutdownSignal(tokio::sync::watch::Sender<bool>);
impl ShutdownSignal {
    pub fn new() -> (Self, Shutdown) {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        (Self(sender), Shutdown(receiver))
    }
    pub fn request(&self) {
        self.0.send_replace(true);
    }
}

pub type TaskFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

pub struct BackgroundTask {
    pub name: &'static str,
    pub run: Box<dyn FnOnce(Shutdown) -> TaskFuture + Send + 'static>,
    /// Only explicitly cooperative tasks get a grace period before abort.
    pub cooperative: bool,
}

impl BackgroundTask {
    pub fn new<F, Fut>(name: &'static str, run: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::fallible(name, move || async move {
            run().await;
            Ok(())
        })
    }

    pub fn fallible<F, Fut>(name: &'static str, run: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            name,
            run: Box::new(move |_| Box::pin(run())),
            cooperative: false,
        }
    }

    pub fn cooperative<F, Fut>(name: &'static str, run: F) -> Self
    where
        F: FnOnce(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            name,
            run: Box::new(move |shutdown| Box::pin(run(shutdown))),
            cooperative: true,
        }
    }
}

impl std::fmt::Debug for BackgroundTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTask")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_is_level_triggered_for_late_and_multiple_waiters() {
        let (signal, mut first) = ShutdownSignal::new();
        let mut second = first.clone();
        assert!(!first.is_requested());
        signal.request();
        signal.request();
        first.requested().await;
        second.requested().await;
        assert!(first.is_requested());
        let (signal, mut receiver) = ShutdownSignal::new();
        drop(signal);
        receiver.requested().await;
        assert!(receiver.is_requested());
    }
}
