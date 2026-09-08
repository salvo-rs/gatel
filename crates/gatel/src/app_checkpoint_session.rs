//! One host lifetime, one checkpoint owner, all production frontend adapters.
use std::path::Path;

use super::*;

/// Explicit checkpoint-only policy, not a WAL/idempotency protocol. Construction
/// performs bounded synchronous disk I/O before entering the async run loop.
/// The CLI must use this session for startup, watcher reloads and Admin creation;
/// exposing App for serving/inspection does not authorize bypass writes.
pub(crate) struct CheckpointSession {
    app: Arc<App>,
    writer: Arc<Mutex<CheckpointWriter>>,
    recovery: Option<(Snapshot, Snapshot)>,
    started: bool,
}

impl CheckpointSession {
    pub(crate) fn open(app: App, directory: &Path, instance: &str) -> Result<Self, &'static str> {
        let capture = app.store.capture_configuration();
        if capture.is_sealed()
            || capture.committed().is_some()
            || capture.observed_snapshot().sources().next().is_some()
        {
            return Err("checkpoint session requires an unused host");
        }
        let (writer, recovery) = CheckpointWriter::open(directory, instance, &app.registry).map_err(
            |_| "cannot open checkpoint session: inspect identity, permissions, lock and recovery state",
        )?;
        Ok(Self {
            app: Arc::new(app),
            writer: Arc::new(Mutex::new(writer)),
            recovery,
            started: false,
        })
    }

    pub(crate) fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// An existing checkpoint is restored without reading current file input.
    /// Keep recovery input on prepare failure, so a corrected resource can retry.
    pub(crate) async fn startup(&mut self) -> Result<Outcome, &'static str> {
        if self.started {
            return Err("checkpoint session is already started");
        }
        let expected = self.app.configuration_status().version();
        let outcome = if let Some((serving, observed)) = &self.recovery {
            self.app
                .recover_checkpoint_if(expected, serving.clone(), observed.clone(), self.writer.clone())
                .await
                .map_err(|_| "checkpoint startup recovery could not be confirmed")?
        } else {
            self.app
                .reload_frontend_checkpoint_if(expected, self.writer.clone())
                .await
                .map_err(|_| "checkpoint startup may be applied but persistence could not be confirmed")?
        };
        if matches!(outcome, Outcome::Started { .. }) {
            self.started = true;
            self.recovery = None;
        }
        Ok(outcome)
    }

    pub(crate) async fn reload(&self) -> Result<Outcome, SourceWriteError> {
        if !self.started {
            return Err(SourceWriteError::Unavailable);
        }
        self.app
            .reload_frontend_checkpoint_if(self.app.configuration_status().version(), self.writer.clone())
            .await
    }

    /// Do not expose a writable HTTP endpoint until recovery/activation is
    /// confirmed. The API shares this session's writer, never opens a second one.
    pub(crate) fn admin_api(
        &self,
        token: zeroize::Zeroizing<String>,
    ) -> Result<crate::admin::AdminApi, &'static str> {
        if !self.started {
            return Err("checkpoint session is not started");
        }
        crate::admin::AdminApi::with_checkpoint(&self.app, token, self.writer.clone())
    }
}
