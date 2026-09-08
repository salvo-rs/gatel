//! Classify recovery work without repairing, applying, or discarding any intent.
use std::io;
use std::sync::Arc;

use super::{Decision, Intent, Journal};
use crate::admin::journal::checkpoint::Checkpoint;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum Disposition {
    /// Binding only, no checkpoint. Host still needs its initial file sources.
    Uninitialized,
    /// No journal records beyond the verified checkpoint.
    CheckpointCurrent,
    /// Only rejected intent follows the checkpoint. Preserve it independently
    /// of the serving snapshot so later source composition cannot forget it.
    CheckpointWithRejectedIntent,
    Reconcile(Reason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum Reason {
    IncompleteTail,
    UnknownOutcome,
    MissingCheckpoint,
    AppliedAfterCheckpoint,
}

/// Trusted recovery input; Debug relies on redacted nested intent/checkpoint
/// implementations. No readiness, runtime activation or durable receipt implied.
#[derive(Debug)]
pub(in super::super) struct Inspection {
    pub disposition: Disposition,
    pub checkpoint: Option<Checkpoint>,
    pub latest: Option<Arc<Intent>>,
    pub applied: Option<Arc<Intent>>,
    pub decision: Option<Decision>,
    pub incomplete_tail: bool,
    pub journal_sequence: u64,
}

impl Journal {
    /// Holds the original writer lock and validates checkpoint/log before
    /// classification. Corruption is an error, never a fresh-start fallback.
    /// If both a tail and unknown outcome exist, tail repair is the first step;
    /// latest/decision retain the unresolved transaction for the next inspection.
    pub(in super::super) fn inspect_recovery(
        &mut self,
        registry: &gatel_plan::Registry,
    ) -> io::Result<Inspection> {
        let checkpoint = self.load_checkpoint(registry)?;
        let journal_sequence = self.store.recovered().sequence;
        let incomplete_tail = self.has_incomplete_tail();
        let disposition = if incomplete_tail {
            Disposition::Reconcile(Reason::IncompleteTail)
        } else if self.state.latest.is_some() && self.state.decision.is_none() {
            Disposition::Reconcile(Reason::UnknownOutcome)
        } else if let Some(checkpoint) = &checkpoint {
            if self
                .state
                .applied
                .as_ref()
                .is_some_and(|intent| intent.sequence > checkpoint.metadata.journal_sequence)
            {
                Disposition::Reconcile(Reason::AppliedAfterCheckpoint)
            } else if journal_sequence > checkpoint.metadata.journal_sequence {
                Disposition::CheckpointWithRejectedIntent
            } else {
                Disposition::CheckpointCurrent
            }
        } else if self.state.latest.is_some() {
            Disposition::Reconcile(Reason::MissingCheckpoint)
        } else {
            Disposition::Uninitialized
        };
        Ok(Inspection {
            disposition,
            checkpoint,
            latest: self.state.latest.clone(),
            applied: self.state.applied.clone(),
            decision: self.state.decision,
            incomplete_tail,
            journal_sequence,
        })
    }
}
