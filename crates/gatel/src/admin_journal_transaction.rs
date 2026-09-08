//! Intent/decision protocol over the exclusively owned append store.
//! A decision is trusted host input, not proof of runtime publication. No HTTP
//! integration, cross-source checkpoint or idempotency receipt is provided yet.
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::Error;
use super::store::Store;

#[path = "admin_recovery.rs"]
pub(super) mod recovery;

const INTENT: &str = "GATEL-TX1 I\n";
const APPLIED: &str = "GATEL-TX1 A\n";
const REJECTED: &str = "GATEL-TX1 R\n";
const DECISION_RESERVE: usize = super::HEADER + APPLIED.len() + 20; // max u64 decimal

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Applied,
    Rejected,
}

pub(super) struct Intent {
    pub sequence: u64,
    pub text: String,
}

impl std::fmt::Debug for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Intent")
            .field("sequence", &self.sequence)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct State {
    /// Retained even when rejected; this is not the serving configuration.
    pub latest: Option<Arc<Intent>>,
    /// Last explicitly confirmed admin contribution, not a full-runtime LKG.
    pub applied: Option<Arc<Intent>>,
    /// None with a latest intent means unknown outcome, never implicit rejection.
    pub decision: Option<Decision>,
}

impl State {
    pub(super) fn accept(&mut self, sequence: u64, payload: &str) -> Result<(), Error> {
        if let Some(text) = payload.strip_prefix(INTENT) {
            if text.len() > super::super::MAX_BODY || (self.latest.is_some() && self.decision.is_none()) {
                return Err(Error::InvalidRecord);
            }
            self.latest = Some(Arc::new(Intent {
                sequence,
                text: text.to_owned(),
            }));
            self.decision = None;
            return Ok(());
        }
        let (decision, reference) = if let Some(reference) = payload.strip_prefix(APPLIED) {
            (Decision::Applied, reference)
        } else if let Some(reference) = payload.strip_prefix(REJECTED) {
            (Decision::Rejected, reference)
        } else {
            return Err(Error::InvalidRecord);
        };
        let referenced: u64 = reference.parse().map_err(|_| Error::InvalidRecord)?;
        // Canonical decimal only: no signs, whitespace, leading zeros or extra
        // fields that a later reader might interpret differently.
        if reference != referenced.to_string()
            || self.decision.is_some()
            || self
                .latest
                .as_ref()
                .is_none_or(|intent| intent.sequence != referenced)
        {
            return Err(Error::InvalidRecord);
        }
        if decision == Decision::Applied {
            self.applied = self.latest.clone();
        }
        self.decision = Some(decision);
        Ok(())
    }
}

pub(super) struct Journal {
    store: Store,
    state: State,
    instance: String,
}

impl Journal {
    pub(super) fn compact_to(
        &mut self,
        registry: &gatel_plan::Registry,
        destination: &Path,
    ) -> io::Result<()> {
        self.store.compact_to(&self.instance, registry, destination)
    }

    pub(super) fn open_existing(directory: &Path, instance: &str) -> io::Result<Self> {
        let mut state = State::default();
        let store = super::binding::open_existing(directory, instance, |sequence, text| {
            state.accept(sequence, text)
        })?;
        Ok(Self {
            store,
            state,
            instance: instance.to_owned(),
        })
    }

    pub(super) fn open(directory: &Path, instance: &str) -> io::Result<Self> {
        let mut state = State::default();
        let store = super::binding::open(directory, instance, |sequence, text| state.accept(sequence, text))?;
        Ok(Self {
            store,
            state,
            instance: instance.to_owned(),
        })
    }

    pub(super) fn state(&self) -> &State {
        &self.state
    }

    /// Host must coordinate this atomic capture with all configuration writers,
    /// completed activation, and the journal decision boundary.
    pub(super) fn save_checkpoint<R>(
        &mut self,
        registry: &gatel_plan::Registry,
        capture: &gatel_plan::ConfigurationCapture<R>,
    ) -> io::Result<()> {
        self.store.save_checkpoint(&self.instance, registry, capture)
    }

    pub(super) fn load_checkpoint(
        &mut self,
        registry: &gatel_plan::Registry,
    ) -> io::Result<Option<super::checkpoint::Checkpoint>> {
        self.store.load_checkpoint(&self.instance, registry)
    }

    pub(super) fn has_incomplete_tail(&self) -> bool {
        self.store.recovered().incomplete_tail
    }

    /// Explicit recovery cannot manufacture a decision for an unfinished intent.
    pub(super) fn discard_incomplete_tail(&mut self) -> io::Result<usize> {
        self.store.discard_incomplete_tail()
    }

    /// Sync intent before the future host starts work. Only one unresolved
    /// intent is allowed; an unknown outcome requires explicit reconciliation.
    pub(super) fn begin(&mut self, text: &str) -> io::Result<u64> {
        if text.len() > super::super::MAX_BODY {
            return Err(invalid());
        }
        // Never consume the space needed to settle this intent. This prevents
        // the configured log cap from stranding a pending transaction; actual
        // filesystem exhaustion can still make any append fail.
        if super::HEADER + INTENT.len() + text.len() + DECISION_RESERVE > self.store.remaining_capacity() {
            return Err(io::Error::other(
                "journal capacity cannot hold intent and decision; log compaction or rotation required; offline compaction needs a fully covered checkpoint",
            ));
        }
        self.append(&format!("{INTENT}{text}"))
    }

    /// Host must only confirm after its publication/activation requirements,
    /// or reject after proving no application. Never call Rejected merely
    /// because a waiter was canceled or an I/O error made the outcome unknown.
    pub(super) fn finish(&mut self, intent: u64, decision: Decision) -> io::Result<u64> {
        let prefix = match decision {
            Decision::Applied => APPLIED,
            Decision::Rejected => REJECTED,
        };
        self.append(&format!("{prefix}{intent}"))
    }

    fn append(&mut self, payload: &str) -> io::Result<u64> {
        let sequence = self
            .store
            .recovered()
            .sequence
            .checked_add(1)
            .ok_or_else(invalid)?;
        let mut candidate = self.state.clone();
        candidate.accept(sequence, payload).map_err(|_| invalid())?;
        // Stage without changing visible state. I/O failure poisons Store and
        // leaves this state at its last known point; reopening is mandatory.
        let written = self.store.append(payload)?;
        self.state = candidate;
        Ok(written)
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Error::InvalidRecord)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::journal::{encode, recover_each};

    // All fixtures are fresh private directories; no operator journal is edited.
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gatel-tx-{}", gatel_model::BootId::generate()));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            builder.create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn applied_rejected_pending_and_empty_contributions_survive_reopen() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert!(journal.state().latest.is_none());
        assert!(journal.state().applied.is_none());
        let first = journal.begin("secret-first;").unwrap();
        assert!(journal.state().applied.is_none());
        journal.finish(first, Decision::Applied).unwrap();
        let second = journal.begin("invalid-config;").unwrap();
        journal.finish(second, Decision::Rejected).unwrap();
        drop(journal);
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert_eq!(journal.state().latest.as_ref().unwrap().text, "invalid-config;");
        assert_eq!(journal.state().decision, Some(Decision::Rejected));
        assert_eq!(journal.state().applied.as_ref().unwrap().text, "secret-first;");
        let pending = journal.begin("").unwrap();
        drop(journal);
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert!(journal.state().decision.is_none());
        assert_eq!(journal.state().latest.as_ref().unwrap().text, "");
        assert_eq!(journal.state().applied.as_ref().unwrap().sequence, first);
        assert!(!format!("{:?}", journal.state()).contains("secret-first"));
        assert!(journal.begin("must-not-hide-unknown;").is_err());
        journal.finish(pending, Decision::Applied).unwrap();
        drop(journal);
        let journal = Journal::open(&directory.0, "fixture").unwrap();
        assert_eq!(journal.state().applied.as_ref().unwrap().text, "");
    }

    #[test]
    fn invalid_transitions_do_not_write_or_poison_valid_writer() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert!(journal.finish(1, Decision::Applied).is_err());
        let intent = journal.begin("first;").unwrap();
        let path = directory.0.join("intent.log");
        let before = std::fs::read(&path).unwrap();
        assert!(journal.begin("second;").is_err());
        assert!(journal.finish(intent + 1, Decision::Applied).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        journal.finish(intent, Decision::Rejected).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(journal.finish(intent, Decision::Rejected).is_err());
        assert!(journal.finish(intent, Decision::Applied).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(journal.begin("next;").is_ok());
    }

    #[test]
    fn checksummed_semantic_corruption_and_noncanonical_references_fail_closed() {
        for payload in [
            "GATEL-TX2 A\n2",
            "GATEL-TX1 A\n02",
            "GATEL-TX1 A\n+2",
            "GATEL-TX1 A\n2\n",
            "GATEL-TX1 A\n0",
            "GATEL-TX1 A\n3",
            "GATEL-TX1 I\nsecond;",
            "raw-legacy-kdl;",
        ] {
            let directory = Directory::new();
            let mut journal = Journal::open(&directory.0, "fixture").unwrap();
            journal.begin("first;").unwrap();
            drop(journal);
            let path = directory.0.join("intent.log");
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend(encode(3, payload).unwrap());
            std::fs::write(&path, &bytes).unwrap();
            assert!(
                Journal::open(&directory.0, "fixture").is_err(),
                "payload={payload}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn every_partial_decision_is_unknown_not_applied_or_rejected() {
        let first = encode(1, &format!("{INTENT}配置;")).unwrap();
        for prefix in [APPLIED, REJECTED] {
            let receipt = encode(2, &format!("{prefix}1")).unwrap();
            for cut in 0..receipt.len() {
                let mut log = first.clone();
                log.extend_from_slice(&receipt[..cut]);
                let mut state = State::default();
                let recovered = recover_each(&log, 0, |seq, text| state.accept(seq, text)).unwrap();
                assert_eq!(recovered.incomplete_tail, cut != 0);
                assert!(state.decision.is_none());
                assert!(state.applied.is_none());
                assert_eq!(state.latest.unwrap().text, "配置;");
            }
        }
    }

    #[test]
    fn tail_repair_does_not_resolve_unknown_decision() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        let intent = journal.begin("first;").unwrap();
        drop(journal);
        let path = directory.0.join("intent.log");
        let mut bytes = std::fs::read(&path).unwrap();
        let receipt = encode(3, &format!("{APPLIED}{intent}")).unwrap();
        bytes.extend_from_slice(&receipt[..receipt.len() - 1]);
        std::fs::write(&path, bytes).unwrap();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert!(journal.has_incomplete_tail());
        assert!(journal.finish(intent, Decision::Applied).is_err());
        assert_eq!(journal.discard_incomplete_tail().unwrap(), receipt.len() - 1);
        assert!(!journal.has_incomplete_tail());
        assert!(journal.state().decision.is_none());
        assert!(journal.begin("second;").is_err());
        journal.finish(intent, Decision::Applied).unwrap();
        assert_eq!(journal.state().applied.as_ref().unwrap().text, "first;");
    }

    #[test]
    fn failed_decision_append_does_not_advance_visible_state_or_allow_retry() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        let intent = journal.begin("first;").unwrap();
        let path = directory.0.join("intent.log");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"GAT");
        std::fs::write(&path, &bytes).unwrap();
        assert!(journal.finish(intent, Decision::Applied).is_err());
        assert!(journal.state().applied.is_none());
        assert!(journal.state().decision.is_none());
        assert!(
            journal
                .finish(intent, Decision::Rejected)
                .unwrap_err()
                .to_string()
                .contains("poisoned")
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(journal);
        let journal = Journal::open(&directory.0, "fixture").unwrap();
        assert!(journal.has_incomplete_tail());
        assert!(journal.state().decision.is_none());
    }

    #[test]
    fn every_partial_next_intent_retains_confirmed_contribution() {
        let mut prefix = encode(1, &format!("{INTENT}first;")).unwrap();
        prefix.extend(encode(2, &format!("{APPLIED}1")).unwrap());
        let next = encode(3, &format!("{INTENT}配置;")).unwrap();
        for cut in 0..next.len() {
            let mut bytes = prefix.clone();
            bytes.extend_from_slice(&next[..cut]);
            let mut state = State::default();
            let recovered = recover_each(&bytes, 0, |seq, text| state.accept(seq, text)).unwrap();
            assert_eq!(recovered.sequence, 2);
            assert_eq!(state.decision, Some(Decision::Applied));
            assert_eq!(state.latest.as_ref().unwrap().sequence, 1);
            assert_eq!(state.applied.as_ref().unwrap().text, "first;");
        }
    }

    #[test]
    fn begin_reserves_room_for_a_decision_at_log_capacity() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        let large = "x".repeat(crate::admin::MAX_BODY);
        for _ in 0..15 {
            let intent = journal.begin(&large).unwrap();
            journal.finish(intent, Decision::Rejected).unwrap();
        }
        let available = journal.store.remaining_capacity();
        let fits = available - DECISION_RESERVE - super::super::HEADER - INTENT.len();
        assert!(fits < crate::admin::MAX_BODY);
        let path = directory.0.join("intent.log");
        let before = std::fs::metadata(&path).unwrap().len();
        let error = journal.begin(&"x".repeat(fits + 1)).unwrap_err();
        assert!(error.to_string().contains("log compaction or rotation required"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
        assert_eq!(journal.state().decision, Some(Decision::Rejected));
        let intent = journal.begin(&"x".repeat(fits)).unwrap();
        assert_eq!(journal.store.remaining_capacity(), DECISION_RESERVE);
        journal.finish(intent, Decision::Applied).unwrap();
        drop(journal);
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        assert_eq!(journal.state().decision, Some(Decision::Applied));
        assert!(journal.begin("").is_err());
        let log = std::fs::read(&path).unwrap();
        let registry = gatel_plan::Registry::builder()
            .bundle(gatel_bundle_std::bundle())
            .build()
            .unwrap();
        let generations = gatel_plan::GenerationStore::<()>::new();
        let text =
            "global { http \"127.0.0.1:8080\"; }\nsite \"*\" { route \"/\" { respond \"checkpoint\"; } }";
        let mut hash = gatel_model::Hasher::new();
        hash.update_str(text);
        let snapshot = gatel_model::Snapshot::new().with(gatel_model::SourceSnapshot::new(
            gatel_model::SourceId::file("main"),
            gatel_model::SourceRev(1),
            hash.finish(),
            gatel_kdl::lower(text, "fixture").unwrap(),
        ));
        assert!(matches!(
            generations.attempt(&snapshot, &registry, |_| Ok(())),
            gatel_plan::Outcome::Started { .. }
        ));
        // A checkpoint covers this full log; it does not rotate it. Rejecting
        // admission must leave a usable writer for inspection/checkpoint save.
        journal
            .save_checkpoint(&registry, &generations.capture_configuration())
            .unwrap();
        assert!(journal.load_checkpoint(&registry).unwrap().is_some());
        assert!(journal.begin("").is_err());
        assert_eq!(journal.state().decision, Some(Decision::Applied));
        assert_eq!(std::fs::read(&path).unwrap(), log);
        drop(journal);
        let mut reopened = Journal::open(&directory.0, "fixture").unwrap();
        assert!(reopened.load_checkpoint(&registry).unwrap().is_some());
        assert!(reopened.begin("").is_err());
        assert_eq!(std::fs::read(path).unwrap(), log);
    }

    #[test]
    fn envelope_preserves_full_body_limit_and_bounds_replayed_intents() {
        let directory = Directory::new();
        let mut journal = Journal::open(&directory.0, "fixture").unwrap();
        let text = "x".repeat(crate::admin::MAX_BODY);
        assert!(journal.begin(&(text.clone() + "x")).is_err());
        let intent = journal.begin(&text).unwrap();
        journal.finish(intent, Decision::Applied).unwrap();
        drop(journal);
        assert_eq!(
            Journal::open(&directory.0, "fixture")
                .unwrap()
                .state()
                .applied
                .as_ref()
                .unwrap()
                .text
                .len(),
            text.len()
        );
        // Framing allowance must not raise the semantic KDL limit.
        let payload = format!("{INTENT}{text}x");
        let bytes = encode(1, &payload).unwrap();
        let mut state = State::default();
        assert_eq!(
            recover_each(&bytes, 0, |seq, text| state.accept(seq, text)).unwrap_err(),
            Error::InvalidRecord
        );
    }
}
