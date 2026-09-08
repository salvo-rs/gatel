//! One atomic observation of published provenance and latest admitted intent.
use std::sync::Arc;

use super::{Committed, GenerationStore, Snapshot, Version};

/// Trusted host input for checkpoint coordination. The published source union
/// and observed union may intentionally differ after NoOp, rejection, or while
/// preparation is in flight. Neither is a substitute for the other.
///
/// Capturing does not drain pending attempts, fence later writers, prove host
/// activation or establish durability. Hosts must capture inside their owned
/// execution gate and coordinate the journal separately before persisting.
/// Fields cannot be assembled from independent store reads by callers.
pub struct ConfigurationCapture<R> {
    version: Version,
    committed: Option<Arc<Committed<R>>>,
    observed: Snapshot,
    sealed: bool,
}

impl<R> ConfigurationCapture<R> {
    /// Write version at capture time; not a receipt for any individual request.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// The generation published at the same instant, if initialization succeeded.
    pub fn committed(&self) -> Option<&Arc<Committed<R>>> {
        self.committed.as_ref()
    }

    /// All latest admitted sources, including rejected intent and tombstones.
    /// May contain secrets. Do not serialize this into public status responses.
    pub fn observed_snapshot(&self) -> &Snapshot {
        &self.observed
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
}

impl<R> std::fmt::Debug for ConfigurationCapture<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigurationCapture")
            .field("generation", &self.version.generation)
            .field("has_committed", &self.committed.is_some())
            .field("observed_sources", &self.observed.sources().count())
            .field("sealed", &self.sealed)
            .finish()
    }
}

impl<R> GenerationStore<R> {
    /// Pin version, runtime provenance and full intent under one read lock.
    /// Only Arc handles/revision metadata are copied under the lock; potentially
    /// large trees are cloned after release. Retained captures keep old runtime
    /// resources and secret-bearing sources alive until their owners drop them.
    pub fn capture_configuration(&self) -> ConfigurationCapture<R> {
        let (version, committed, sources, sealed) = {
            let state = self.read();
            (
                Version {
                    boot_id: state.boot_id,
                    generation: state.generation,
                    revs: state.head.clone(),
                },
                state.current.clone(),
                state.intent.values().cloned().collect::<Vec<_>>(),
                state.sealed,
            )
        };
        let mut observed = Snapshot::new();
        for source in sources {
            observed.insert((*source).clone());
        }
        ConfigurationCapture {
            version,
            committed,
            observed,
            sealed,
        }
    }
}

#[cfg(test)]
#[path = "generation_capture_tests.rs"]
mod tests;
