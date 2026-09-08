//! Narrow host checkpoint capability; does not expose raw journal mutation.
use std::io;
use std::path::Path;

use gatel_model::{BootId, Fingerprint, Hasher, Snapshot};
use gatel_plan::{ConfigurationCapture, Registry};

use super::transaction::Journal;
use super::transaction::recovery::Disposition;

pub(crate) struct CheckpointWriter {
    journal: Journal,
    // Host publication may already have happened even when checkpoint encoding
    // fails before disk I/O. Never accept another transaction from this handle
    // until the host has reopened and reconciled its in-memory/disk state.
    failed: bool,
    owner: Option<BootId>,
    recovery: Option<(Fingerprint, Fingerprint)>,
}

impl CheckpointWriter {
    /// Existing private directory. May initialize binding, but never repair or
    /// ignore later journal records. Caller must recover returned snapshots
    /// before replacing an existing checkpoint and must serialize all writers.
    pub(crate) fn open(
        directory: &Path,
        instance: &str,
        registry: &Registry,
    ) -> io::Result<(Self, Option<(Snapshot, Snapshot)>)> {
        let mut journal = Journal::open(directory, instance)?;
        let inspection = journal.inspect_recovery(registry)?;
        if !matches!(
            inspection.disposition,
            Disposition::Uninitialized | Disposition::CheckpointCurrent
        ) {
            return Err(io::Error::other(
                "checkpoint writer requires explicit journal reconciliation",
            ));
        }
        let snapshots = inspection
            .checkpoint
            .map(|checkpoint| (checkpoint.sources, checkpoint.observed));
        let recovery = snapshots
            .as_ref()
            .map(|(serving, observed)| identity(serving, observed))
            .transpose()?;
        Ok((
            Self {
                journal,
                failed: false,
                owner: None,
                recovery,
            },
            snapshots,
        ))
    }

    /// Preflight for the host's serialized execution gate, before publishing.
    /// This is not a reservation and does not make a later save infallible.
    pub(crate) fn ensure_ready(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "checkpoint writer requires reopen and reconciliation",
            ));
        }
        if self.recovery.is_some() {
            return Err(io::Error::other(
                "checkpoint recovery must be confirmed before writing",
            ));
        }
        Ok(())
    }

    /// Prove full serving provenance and observed intent, not just revisions or
    /// normalized plan equivalence. Run after recovery under host execution
    /// ownership; activation is still the host's separate responsibility.
    pub(crate) fn confirm_recovery<R>(&mut self, capture: &ConfigurationCapture<R>) -> io::Result<()> {
        if self.failed || capture.is_sealed() {
            return Err(io::Error::other("checkpoint recovery cannot be confirmed"));
        }
        if self.owner.is_some_and(|owner| owner != capture.version().boot_id) {
            return Err(io::Error::other(
                "checkpoint recovery belongs to another host lifetime",
            ));
        }
        let serving = capture
            .committed()
            .ok_or_else(|| io::Error::other("checkpoint recovery has no serving generation"))?;
        self.verify_recovery_sources(serving.source_snapshot(), capture.observed_snapshot())?;
        self.owner = Some(capture.version().boot_id);
        self.recovery = None;
        Ok(())
    }

    /// Pre-publication validation retains pending recovery until confirmation.
    pub(crate) fn verify_recovery_sources(&self, serving: &Snapshot, observed: &Snapshot) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("checkpoint writer requires reconciliation"));
        }
        let expected = self
            .recovery
            .ok_or_else(|| io::Error::other("no checkpoint recovery is pending"))?;
        if identity(serving, observed)? != expected {
            return Err(io::Error::other(
                "recovered source snapshots do not match checkpoint",
            ));
        }
        Ok(())
    }

    /// Reserve pending recovery before publication; another App must not pass
    /// preflight concurrently using the same still-unconfirmed capability.
    pub(crate) fn reserve_recovery(
        &mut self,
        owner: BootId,
        serving: &Snapshot,
        observed: &Snapshot,
    ) -> io::Result<()> {
        self.verify_recovery_sources(serving, observed)?;
        if self.owner.is_some_and(|bound| bound != owner) {
            return Err(io::Error::other(
                "checkpoint recovery belongs to another host lifetime",
            ));
        }
        self.owner = Some(owner);
        Ok(())
    }

    /// One opened capability belongs to one host lifetime. OS file locking does
    /// not serialize two App execution gates sharing the same Rust handle.
    /// A foreign host is rejected without poisoning the legitimate owner.
    pub(crate) fn bind(&mut self, owner: BootId) -> io::Result<()> {
        self.ensure_ready()?;
        if self.owner.is_some_and(|bound| bound != owner) {
            return Err(io::Error::other(
                "checkpoint writer belongs to another host lifetime",
            ));
        }
        self.owner = Some(owner);
        Ok(())
    }

    /// Host could not establish a safe checkpoint after changing memory.
    pub(crate) fn require_reconciliation(&mut self) {
        self.failed = true;
    }

    /// Call inside owned blocking completion, after the host's activation and
    /// outcome checks. I/O errors may follow replacement: never report unapplied.
    pub(crate) fn save<R>(
        &mut self,
        registry: &Registry,
        capture: &ConfigurationCapture<R>,
    ) -> io::Result<()> {
        self.bind(capture.version().boot_id)?;
        self.failed = true;
        self.journal.save_checkpoint(registry, capture)?;
        self.failed = false;
        Ok(())
    }
}

fn identity(serving: &Snapshot, observed: &Snapshot) -> io::Result<(Fingerprint, Fingerprint)> {
    fn digest(snapshot: &Snapshot) -> io::Result<Fingerprint> {
        let bytes = gatel_model::snapshot_codec::encode(snapshot)
            .map_err(|_| io::Error::other("cannot identify checkpoint sources"))?;
        let mut hash = Hasher::new();
        hash.update_str("gatel.checkpoint.recovery-sources.v1");
        hash.update_bytes(&bytes);
        Ok(hash.finish())
    }
    Ok((digest(serving)?, digest(observed)?))
}
