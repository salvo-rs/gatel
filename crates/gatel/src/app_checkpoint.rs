//! Complete snapshot submission with a serialized post-activation checkpoint.
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use gatel_model::Snapshot;
use gatel_plan::{Conflict, Outcome, SourceWriteError, Version};

use super::App;
use crate::admin::CheckpointWriter;

#[path = "app_checkpoint_session.rs"]
pub(super) mod session;

#[derive(Debug)]
pub(super) enum CheckpointRecoveryError {
    Restore(gatel_plan::RestoreIntentError),
    Unavailable,
    WriterUnavailable,
    ConfirmationUnknown,
}

impl std::fmt::Display for CheckpointRecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Restore(error) => error.fmt(f),
            Self::Unavailable => f.write_str("checkpoint recovery admission is closed"),
            Self::WriterUnavailable => f.write_str("checkpoint recovery input or writer is unavailable"),
            Self::ConfirmationUnknown => {
                f.write_str("checkpoint recovery may be published but is not confirmed")
            }
        }
    }
}
impl std::error::Error for CheckpointRecoveryError {}

impl App {
    /// Observe the configured file/immutable frontend using the same source
    /// composition and resource-refresh path as ordinary reload. CAS and writer
    /// readiness are checked before reading files or advancing frontend caches.
    #[allow(dead_code, reason = "Checkpointed watcher/startup integration entry")]
    pub(super) async fn reload_frontend_checkpoint_if(
        self: &Arc<Self>,
        expected: Version,
        writer: Arc<Mutex<CheckpointWriter>>,
    ) -> Result<Outcome, SourceWriteError> {
        let preflight = writer.clone();
        let base = expected.clone();
        self.reload_task_persisting(
            move |app| async move {
                app.configuration_work(move |app| {
                    let actual = app.store.status().version();
                    if actual != expected {
                        return Err(SourceWriteError::Conflict(Conflict { actual }));
                    }
                    preflight
                        .lock()
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?
                        .bind(actual.boot_id)
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?;
                    Ok(app.attempt())
                })
                .await
                .unwrap_or(Err(SourceWriteError::Unavailable))
            },
            move |app, result| app.finish_checkpoint(&base, &writer, result),
        )
        .await
    }

    /// Restore and confirm in one owned execution. Confirmation does not write
    /// disk; it enables the verified writer for subsequent checkpoint updates.
    #[allow(dead_code, reason = "Writable checkpoint bootstrap integration entry")]
    pub(super) async fn recover_checkpoint_if(
        self: &Arc<Self>,
        expected: Version,
        serving: Snapshot,
        observed: Snapshot,
        writer: Arc<Mutex<CheckpointWriter>>,
    ) -> Result<Outcome, CheckpointRecoveryError> {
        let preflight = writer.clone();
        self.reload_task_persisting(
            move |app| async move {
                app.configuration_work(move |app| {
                    // Reject stale/active-host requests before claiming the
                    // pending writer. A rejected CAS must have no ownership
                    // side effect that can strand another valid recovery.
                    if expected.generation != gatel_model::Generation::NONE {
                        return Err(CheckpointRecoveryError::Restore(
                            gatel_plan::RestoreIntentError::AlreadyPublished,
                        ));
                    }
                    let actual = app.store.status().version();
                    if actual != expected {
                        return Err(CheckpointRecoveryError::Restore(
                            gatel_plan::RestoreIntentError::Conflict(Conflict { actual }),
                        ));
                    }
                    preflight
                        .lock()
                        .map_err(|_| CheckpointRecoveryError::WriterUnavailable)?
                        .reserve_recovery(actual.boot_id, &serving, &observed)
                        .map_err(|_| CheckpointRecoveryError::WriterUnavailable)?;
                    app.recover_snapshot_if(&expected, &serving, &observed)
                        .map_err(CheckpointRecoveryError::Restore)
                })
                .await
                .unwrap_or(Err(CheckpointRecoveryError::Unavailable))
            },
            move |app, result| {
                let outcome = result?;
                if !matches!(outcome, Outcome::Started { .. }) {
                    return Ok(outcome);
                }
                let mut writer = writer.lock().map_err(|_| {
                    app.telemetry.health.persistence_unknown();
                    CheckpointRecoveryError::ConfirmationUnknown
                })?;
                let capture = app.store.capture_configuration();
                if capture
                    .committed()
                    .is_none_or(|committed| app.activated.load(Ordering::Acquire) < committed.number.number())
                    || writer.confirm_recovery(&capture).is_err()
                {
                    writer.require_reconciliation();
                    app.telemetry.health.persistence_unknown();
                    return Err(CheckpointRecoveryError::ConfirmationUnknown);
                }
                Ok(outcome)
            },
        )
        .await
    }

    /// Trusted host entry, not a source-scoped network API. Caller must recover
    /// this writer's existing snapshots first and route every participating
    /// source through the same policy. No intent WAL or idempotent HTTP receipt
    /// is promised: a crash before save can retain the previous checkpoint.
    #[allow(dead_code, reason = "Checkpointed host entry; CLI policy not yet exposed")]
    pub(super) async fn reload_checkpoint_if(
        self: &Arc<Self>,
        expected: Version,
        snapshot: Snapshot,
        writer: Arc<Mutex<CheckpointWriter>>,
    ) -> Result<Outcome, SourceWriteError> {
        let preflight = writer.clone();
        let base = expected.clone();
        self.reload_task_persisting(
            move |app| async move {
                app.configuration_work(move |app| {
                    let actual = app.store.status().version();
                    if actual != expected {
                        return Err(SourceWriteError::Conflict(Conflict { actual }));
                    }
                    preflight
                        .lock()
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?
                        .bind(actual.boot_id)
                        .map_err(|_| SourceWriteError::PersistenceUnknown)?;
                    app.attempt_snapshot_if(&expected, &snapshot)
                        .map_err(SourceWriteError::Conflict)
                })
                .await
                .unwrap_or(Err(SourceWriteError::Unavailable))
            },
            move |app, result| app.finish_checkpoint(&base, &writer, result),
        )
        .await
    }

    /// Shared completion for full-snapshot and source-scoped writers. Must run
    /// in owned blocking completion, with the host execution gate still held.
    pub(super) fn finish_checkpoint(
        &self,
        base: &Version,
        writer: &Mutex<CheckpointWriter>,
        result: Result<Outcome, SourceWriteError>,
    ) -> Result<Outcome, SourceWriteError> {
        let result = self.persist_checkpoint(base, writer, result);
        if matches!(result, Err(SourceWriteError::PersistenceUnknown)) {
            self.telemetry.health.persistence_unknown();
        }
        result
    }

    fn persist_checkpoint(
        &self,
        base: &Version,
        writer: &Mutex<CheckpointWriter>,
        result: Result<Outcome, SourceWriteError>,
    ) -> Result<Outcome, SourceWriteError> {
        let outcome = result?;
        let mut writer = writer.lock().map_err(|_| SourceWriteError::PersistenceUnknown)?;
        let capture = self.store.capture_configuration();
        // A seal/race can reject publication after intent admission.
        // Unchanged stale work has nothing new to persist.
        if matches!(outcome, Outcome::Stale(_)) && capture.version() == base {
            return Ok(outcome);
        }
        // Cold observation failure has no serving state or retained intent to
        // persist. Keep the diagnostic as a rejection, without poisoning a new
        // writer: a corrected file/inline input can retry in this lifetime.
        // A parsed-but-rejected source advances intent and must not use this
        // exception; it still needs a recoverable checkpoint or reconciliation.
        if matches!(outcome, Outcome::Rejected(_))
            && capture.version() == base
            && capture.committed().is_none()
            && capture.observed_snapshot().sources().next().is_none()
        {
            return Ok(outcome);
        }
        if capture
            .committed()
            .is_none_or(|committed| self.activated.load(Ordering::Acquire) < committed.number.number())
        {
            writer.require_reconciliation();
            return Err(SourceWriteError::PersistenceUnknown);
        }
        writer
            .save(&self.registry, &capture)
            .map_err(|_| SourceWriteError::PersistenceUnknown)?;
        Ok(outcome)
    }
}

#[cfg(test)]
#[path = "app_checkpoint_tests.rs"]
mod tests;
