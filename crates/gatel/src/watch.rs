//! Noticing that the configuration changed.
//!
//! Event driven, with a slow poll behind it: file watching is unreliable across
//! platforms and editors — some replace a file rather than writing it, some
//! move it into place — and a gateway that silently stops noticing changes is
//! worse than one that checks occasionally.

use std::sync::Arc;
use std::time::Duration;

use gatel_plan::Outcome;
use notify::{EventKind, RecursiveMode, Watcher};

use crate::app::App;

/// Coalesce a burst of file events into one reload: an editor saving a file
/// often produces several.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often to look even if no event arrived.
const FALLBACK_POLL: Duration = Duration::from_secs(5);

/// Reload until the process is asked to stop.
pub async fn reload_on_change(app: Arc<App>) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(8);

    let watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            )
        {
            let _ = sender.try_send(());
        }
    }) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(
                target: "gatel::config",
                event = "watch_unavailable",
                %error,
                "falling back to polling",
            );
            None
        }
    };

    let mut watcher = watcher;
    if let Some(watcher) = watcher.as_mut() {
        // Watching the directory rather than each file survives editors that
        // replace a file instead of writing into it.
        let directory = app.root_directory().to_path_buf();
        if let Err(error) = watcher.watch(&directory, RecursiveMode::Recursive) {
            tracing::warn!(target: "gatel::config", event = "watch_failed", %error);
        }
        tracing::debug!(
            target: "gatel::config",
            event = "watching",
            directory = %directory.display(),
            files = app.watched_paths().len(),
        );
    }

    loop {
        let changed = tokio::select! {
            _ = shutdown_signal() => break,
            _ = receiver.recv() => true,
            _ = tokio::time::sleep(FALLBACK_POLL) => true,
        };
        if !changed {
            continue;
        }

        // Drain the rest of the burst before doing any work.
        tokio::time::sleep(DEBOUNCE).await;
        while receiver.try_recv().is_ok() {}

        reload_once(&app).await;
    }
}

/// Run one attempt and report what it did.
pub async fn reload_once(app: &Arc<App>) -> Outcome {
    let outcome = app.attempt();
    match &outcome {
        Outcome::NoOp { .. } => {}
        Outcome::Started { generation } | Outcome::Reloaded { generation, .. } => {
            app.activate().await;
            tracing::info!(
                target: "gatel::config",
                event = "config_committed",
                generation = generation.number(),
            );
        }
        Outcome::Rejected(diagnostics) => {
            tracing::warn!(
                target: "gatel::config",
                event = "config_rejected",
                problems = diagnostics.error_count(),
                serving = app.store.status().is_serving(),
            );
            eprintln!("{diagnostics}");
        }
        Outcome::PrepareFailed(message) => {
            tracing::warn!(
                target: "gatel::config",
                event = "prepare_failed",
                detail = %message,
                serving = app.store.status().is_serving(),
            );
        }
        Outcome::Stale(reason) => {
            tracing::debug!(target: "gatel::config", event = "candidate_stale", reason = %reason);
        }
    }
    outcome
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT is always available");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM is always available");
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // Windows has no SIGTERM; Ctrl-C and the service control handler are the
    // ways a process is asked to stop.
    let _ = tokio::signal::ctrl_c().await;
}
