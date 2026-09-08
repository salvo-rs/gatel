//! Trusted recovery admission without compiling or publishing recovered intent.
use std::sync::Arc;

use super::{Conflict, GenerationStore, Snapshot, Version};

#[derive(Debug)]
pub enum RestoreIntentError {
    Conflict(Conflict),
    Sealed,
    InvalidSources,
    AlreadyPublished,
}

impl std::fmt::Display for RestoreIntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Conflict(_) => "configuration changed during intent restoration",
            Self::Sealed => "configuration publication is sealed",
            Self::InvalidSources => "recovered intent violates source admission or history",
            Self::AlreadyPublished => "checkpoint recovery requires an unpublished store",
        })
    }
}
impl std::error::Error for RestoreIntentError {}

impl<R> GenerationStore<R> {
    /// Bootstrap a fresh host from independent serving and observed snapshots.
    /// Validate both source unions before any resource preparation; retain the
    /// observed union as the candidate fence, but compile/publish only serving.
    /// Failed prepare leaves no published runtime and can be retried with the
    /// newly observed Version. Host activation and journal coordination remain
    /// the caller's responsibility; never use this as a live rollback endpoint.
    pub fn recover_if<P>(
        &self,
        expected: &Version,
        serving: &Snapshot,
        observed: &Snapshot,
        registry: &super::Registry,
        prepare: P,
    ) -> Result<super::Outcome, RestoreIntentError>
    where
        P: FnOnce(super::Preparation<'_, R>) -> Result<R, String>,
        R: crate::publication::PublishRuntime,
    {
        if expected.generation != gatel_model::Generation::NONE {
            return Err(RestoreIntentError::AlreadyPublished);
        }
        self.validate_restored_sources(&Snapshot::new(), serving)?;
        self.validate_restored_sources(serving, observed)?;
        let version = self.restore_intent_if(expected, observed)?;
        Ok(self.finish_attempt(
            serving,
            registry,
            prepare,
            super::CandidateBase {
                expected_head: version.revs,
                base_generation: version.generation,
                previous: None,
                refresh: false,
            },
        ))
    }

    /// Restore a complete, trusted checkpoint intent union without normalizing,
    /// preparing, publishing or activating it. Restore the serving snapshot using
    /// the ordinary transaction first, then call this before exposing writers.
    /// Invalid semantic intent is deliberately retained, not automatically tried.
    ///
    /// Full Version CAS is checked again after cloning/validation outside locks.
    /// Rejection is atomic, including mixed valid/invalid source updates. This
    /// advances the observed head, fences older candidates, and retains existing
    /// runtime/BootId/generation. It is not a client write or durability receipt.
    pub fn restore_intent_if(
        &self,
        expected: &Version,
        snapshot: &Snapshot,
    ) -> Result<Version, RestoreIntentError> {
        self.restore_intent_with(expected, snapshot, || {})
    }

    fn restore_intent_with(
        &self,
        expected: &Version,
        snapshot: &Snapshot,
        before_commit: impl FnOnce(),
    ) -> Result<Version, RestoreIntentError> {
        let (actual, previous) = self.observed_snapshot();
        if &actual != expected {
            return Err(RestoreIntentError::Conflict(Conflict { actual }));
        }
        self.validate_restored_sources(&previous, snapshot)?;
        self.commit_restored_intent(expected, snapshot, before_commit)
    }

    fn validate_restored_sources(
        &self,
        previous: &Snapshot,
        snapshot: &Snapshot,
    ) -> Result<(), RestoreIntentError> {
        if snapshot.sources().count() > self.source_limit.get()
            || snapshot.sources().any(|source| {
                self.allowed_sources
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&source.source))
            })
        {
            return Err(RestoreIntentError::InvalidSources);
        }
        previous
            .validate_successor(snapshot)
            .map_err(|_| RestoreIntentError::InvalidSources)
    }

    fn commit_restored_intent(
        &self,
        expected: &Version,
        snapshot: &Snapshot,
        before_commit: impl FnOnce(),
    ) -> Result<Version, RestoreIntentError> {
        let intent = snapshot
            .sources()
            .map(|source| (source.source.clone(), Arc::new(source.clone())))
            .collect();
        let head = snapshot.revs();
        // Deterministic race seam; no callback runs with the state lock held.
        before_commit();
        let (retired, version) = {
            let mut state = self.write();
            if state.sealed {
                return Err(RestoreIntentError::Sealed);
            }
            let actual = Version {
                boot_id: state.boot_id,
                generation: state.generation,
                revs: state.head.clone(),
            };
            if &actual != expected {
                return Err(RestoreIntentError::Conflict(Conflict { actual }));
            }
            let retired = std::mem::replace(&mut state.intent, intent);
            state.head = head;
            let version = Version {
                boot_id: state.boot_id,
                generation: state.generation,
                revs: state.head.clone(),
            };
            (retired, version)
        };
        drop(retired);
        Ok(version)
    }
}

#[cfg(test)]
#[path = "generation_restore_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "generation_recover_tests.rs"]
mod recovery_tests;
