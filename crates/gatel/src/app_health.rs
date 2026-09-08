//! Read transactional failure directly; notifications are not a source of truth.
use std::sync::Weak;

use gatel_observe::health::{ReloadFailure, ReloadStatus};

impl super::App {
    /// Sample activation before publication so the returned watermark cannot
    /// exceed the reported generation. It may lag concurrent activation; this
    /// is historical completion, not proof that listeners are currently live.
    #[cfg(feature = "admin-api")]
    pub(crate) fn configuration_progress(&self) -> (gatel_plan::Status, u64) {
        let activated = self.activated.load(std::sync::atomic::Ordering::Acquire);
        (self.store.status(), activated)
    }

    /// Read-only observation. This does not grant source access or write authority.
    pub(crate) fn configuration_status(&self) -> gatel_plan::Status {
        self.store.status()
    }

    #[cfg(feature = "admin-api")]
    pub(crate) fn configuration_closed(&self) -> bool {
        self.store.is_sealed()
    }

    #[cfg(all(test, feature = "admin-api"))]
    pub(crate) fn published_for_test(
        &self,
    ) -> Option<std::sync::Arc<gatel_plan::Committed<std::sync::Arc<gatel_http::Runtime>>>> {
        self.store.current()
    }
}

pub(super) struct ConfigurationStatus(
    pub Weak<gatel_plan::GenerationStore<std::sync::Arc<gatel_http::Runtime>>>,
);

impl std::fmt::Debug for ConfigurationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigurationStatus")
    }
}
impl ReloadStatus for ConfigurationStatus {
    fn failure(&self) -> Option<ReloadFailure> {
        let failure = self.0.upgrade()?.status().last_failure?;
        Some(ReloadFailure {
            attempts: failure.attempts,
            summary: failure.summary,
        })
    }
}
