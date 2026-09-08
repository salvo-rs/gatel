//! Noticing that the configuration changed.
//!
//! Event driven, with a slow poll behind it: file watching is unreliable across
//! platforms and editors — some replace a file rather than writing it, some
//! move it into place — and a gateway that silently stops noticing changes is
//! worse than one that checks occasionally.

use std::sync::Arc;
use std::time::Duration;

use gatel_plan::Outcome;
use notify::{RecursiveMode, Watcher};

use crate::app::App;

#[path = "watch_directories.rs"]
mod directories;

#[path = "watch_events.rs"]
mod events;

type WatchedNames = Arc<std::sync::RwLock<Option<std::collections::BTreeSet<std::ffi::OsString>>>>;

/// Coalesce a burst of file events into one reload: an editor saving a file
/// often produces several.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often to look even if no event arrived.
const FALLBACK_POLL: Duration = Duration::from_secs(5);

/// Reload until the process is asked to stop.
pub async fn reload_on_change(app: Arc<App>) {
    reload_until(app, shutdown_signal()).await;
}

pub(crate) async fn reload_until(app: Arc<App>, stop: impl std::future::Future<Output = ()>) {
    // One signal subscription survives every await in discovery, debounce,
    // preparation and activation. The caller drains admitted workers afterward.
    tokio::select! {
        biased;
        _ = stop => {},
        _ = watch_configuration(app.clone(), move || {
            let app = app.clone();
            async move { reload_once(&app).await; Ok(()) }
        }) => {},
    }
}

/// The host handles a terminal checkpoint error; never silently fall back to
/// memory-only reload or automatically retry a persistence-unknown result.
#[cfg(feature = "admin-api")]
pub(crate) async fn reload_checkpoint_until(
    session: Arc<crate::app::CheckpointSession>,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), gatel_plan::SourceWriteError> {
    let app = session.app().clone();
    tokio::select! {
        biased;
        _ = stop => Ok(()),
        result = watch_configuration(app, move || {
            let session = session.clone();
            async move {
                match session.reload().await {
                    Ok(outcome) => { report_outcome(session.app(), &outcome); Ok(()) }
                    Err(gatel_plan::SourceWriteError::Conflict(_)) => {
                        // A newer source won before admission. No write was
                        // attempted; normal events/polling can observe again.
                        Ok(())
                    }
                    Err(error) => {
                        tracing::error!(target: "gatel::config", event = "checkpoint_reload_failed",
                            "checkpoint reload cannot be confirmed; stopping watcher, inspect before retrying");
                        Err(error)
                    }
                }
            }
        }) => result,
    }
}

async fn watch_configuration<F, Fut>(app: Arc<App>, reload: F) -> Result<(), gatel_plan::SourceWriteError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), gatel_plan::SourceWriteError>>,
{
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(8);
    let names: WatchedNames = Arc::default();
    let event_names = names.clone();

    let watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && events::relevant(
                &event,
                event_names.read().unwrap_or_else(|p| p.into_inner()).as_ref(),
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
    let mut directories = directories::Directories::default();
    refresh_directories(&app, &mut watcher, &mut directories, &names).await;

    loop {
        wait_for_change(&mut receiver).await;

        // Drain the rest of the burst before doing any work.
        tokio::time::sleep(DEBOUNCE).await;
        while receiver.try_recv().is_ok() {}

        reload().await?;
        refresh_directories(&app, &mut watcher, &mut directories, &names).await;
    }
}

async fn refresh_directories(
    app: &Arc<App>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    directories: &mut directories::Directories,
    names: &WatchedNames,
) {
    if let Some(watcher) = watcher.as_mut() {
        let Some((desired, next_names)) = app
            .configuration_work(|app| {
                let files = app.watched_paths();
                let names = files
                    .iter()
                    .filter_map(|path| path.file_name().map(|name| name.to_os_string()))
                    .collect();
                (directories::Directories::parents(files), names)
            })
            .await
        else {
            return;
        };
        *names.write().unwrap_or_else(|p| p.into_inner()) = Some(next_names);
        directories.reconcile(desired, |directory, add| {
            let result = if add {
                // Every loaded import contributes its own parent. Avoid
                // recursively watching unrelated static content/build trees.
                watcher.watch(directory, RecursiveMode::NonRecursive)
            } else {
                watcher.unwatch(directory)
            };
            match result {
                Ok(()) => {
                    tracing::debug!(target: "gatel::config", event = "watch_updated", directory = %directory.display(), add);
                    true
                }
                Err(error) => {
                    tracing::warn!(target: "gatel::config", event = "watch_update_failed", directory = %directory.display(), add, %error);
                    false
                }
            }
        });
    }
}

async fn wait_for_change(receiver: &mut tokio::sync::mpsc::Receiver<()>) {
    tokio::select! {
        // A failed/dropped watcher closes the channel. None is not a file
        // event: disable this branch and preserve the fallback deadline.
        Some(()) = receiver.recv() => {},
        _ = tokio::time::sleep(FALLBACK_POLL) => {},
    }
}

/// Run one attempt and report what it did.
pub async fn reload_once(app: &Arc<App>) -> Outcome {
    let outcome = app.reload().await;
    report_outcome(app, &outcome);
    outcome
}

fn report_outcome(app: &App, outcome: &Outcome) {
    match outcome {
        Outcome::NoOp { .. } => {}
        Outcome::Started { generation } | Outcome::Reloaded { generation, .. } => {
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
                serving = app.configuration_status().is_serving(),
            );
            eprintln!("{diagnostics}");
        }
        Outcome::PrepareFailed(message) => {
            tracing::warn!(
                target: "gatel::config",
                event = "prepare_failed",
                detail = %message,
                serving = app.configuration_status().is_serving(),
            );
        }
        Outcome::Stale(reason) => {
            tracing::debug!(target: "gatel::config", event = "candidate_stale", reason = %reason);
        }
    }
}

#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT is always available");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM is always available");
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    // Windows has no SIGTERM; Ctrl-C and the service control handler are the
    // ways a process is asked to stop.
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn closed_notification_channel_keeps_the_fallback_poll_interval() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        drop(sender);
        for _ in 0..3 {
            let start = tokio::time::Instant::now();
            wait_for_change(&mut receiver).await;
            assert_eq!(start.elapsed(), FALLBACK_POLL);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queued_events_wake_immediately_and_silence_polls() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        sender.try_send(()).unwrap();
        let start = tokio::time::Instant::now();
        wait_for_change(&mut receiver).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        wait_for_change(&mut receiver).await;
        assert_eq!(start.elapsed(), FALLBACK_POLL);
    }

    #[tokio::test(start_paused = true)]
    async fn closure_during_wait_does_not_restart_or_skip_the_deadline() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let start = tokio::time::Instant::now();
        let close = async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            drop(sender);
        };
        tokio::join!(wait_for_change(&mut receiver), close);
        assert_eq!(start.elapsed(), FALLBACK_POLL);
    }
}
