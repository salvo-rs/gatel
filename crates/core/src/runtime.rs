//! Runtime abstraction layer for gatel.
//!
//! Provides a common interface over different async runtimes. The default
//! implementation uses Tokio. When the `monoio` feature is enabled, a
//! monoio-based implementation is available for Linux systems with io_uring
//! support.
//!
//! # Architecture
//!
//! The abstraction is intentionally thin — it wraps the most commonly used
//! runtime operations (spawning tasks, sleeping, TCP operations) behind
//! feature-gated implementations. Business logic uses these wrappers
//! instead of calling tokio/monoio directly, making it possible to switch
//! runtimes at compile time.
//!
//! # Usage
//!
//! ```rust,ignore
//! use gatel_core::runtime;
//!
//! // Spawn a background task
//! runtime::spawn(async { /* ... */ });
//!
//! // Sleep
//! runtime::sleep(Duration::from_secs(1)).await;
//! ```

use std::future::Future;
use std::time::Duration;

/// Spawn a new async task on the current runtime.
///
/// On tokio: delegates to `tokio::spawn`.
/// On monoio: delegates to `monoio::spawn` (task is !Send, pinned to current thread).
#[cfg(not(feature = "runtime-monoio"))]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

#[cfg(feature = "runtime-monoio")]
pub fn spawn<F>(future: F)
where
    F: Future + 'static,
    F::Output: 'static,
{
    monoio::spawn(future);
}

/// Sleep for the given duration.
#[cfg(not(feature = "runtime-monoio"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(feature = "runtime-monoio")]
pub async fn sleep(duration: Duration) {
    monoio::time::sleep(duration).await;
}

/// Create a periodic interval timer.
#[cfg(not(feature = "runtime-monoio"))]
pub fn interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval(period)
}

/// Runtime information and capabilities.
pub struct RuntimeInfo {
    /// Name of the active runtime.
    pub name: &'static str,
    /// Whether io_uring is available (Linux only).
    pub io_uring: bool,
    /// Whether tasks are Send (tokio=true, monoio=false).
    pub send_tasks: bool,
}

/// Get information about the active runtime.
pub fn info() -> RuntimeInfo {
    #[cfg(not(feature = "runtime-monoio"))]
    {
        RuntimeInfo {
            name: "tokio",
            io_uring: false,
            send_tasks: true,
        }
    }
    #[cfg(feature = "runtime-monoio")]
    {
        RuntimeInfo {
            name: "monoio",
            io_uring: cfg!(target_os = "linux"),
            send_tasks: false,
        }
    }
}
