//! Dual-source checkpoint paired with an exact bound journal prefix. No disk I/O
//! or activation receipt: the future host must serialize capture with all writers.
use std::io;
use std::sync::Arc;

use gatel_model::{Fingerprint, Hasher, Snapshot, snapshot_codec};
use gatel_plan::{ConfigurationCapture, Registry, compile, normalize};

use super::transaction::State;
use super::{binding, checksum, recover_each};

const MAGIC: &[u8; 8] = b"GATELC02";
const HEADER: usize = 40;
// instance, build, plan, publication, sequence, prefix length, prefix hash.
const META: usize = 32 * 5 + 8 * 2;
const LENGTH: usize = 8;
pub(super) const MAX_BYTES: usize = HEADER + META + LENGTH + 2 * snapshot_codec::MAX_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    pub instance: Fingerprint,
    pub build: Fingerprint,
    pub plan: Fingerprint,
    /// Domain hash of the originating BootId + generation, never a new BootId.
    pub publication: Fingerprint,
    pub journal_sequence: u64,
    pub journal_bytes: u64,
    pub journal_prefix: Fingerprint,
}

pub(super) struct Checkpoint {
    pub metadata: Metadata,
    pub sources: Snapshot,
    /// Latest admitted complete union, not necessarily valid or serving.
    pub observed: Snapshot,
}
impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoint")
            .field("metadata", &self.metadata)
            .field("source_count", &self.sources.sources().count())
            .field("observed_source_count", &self.observed.sources().count())
            .finish()
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or incompatible configuration checkpoint",
    )
}

/// Rebind a fully covered checkpoint to a fresh binding-only journal. Preserve
/// the complete snapshots and originating publication identity byte-for-byte.
/// This is offline history compaction, not intent reconciliation or activation.
pub(super) fn compact(
    bytes: &[u8],
    instance: &str,
    registry: &Registry,
    log: &[u8],
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let checkpoint = decode(bytes, instance, registry, log)?;
    let (recovered, state) = log_state(log, instance)?;
    if recovered.incomplete_tail
        || checkpoint.metadata.journal_bytes != log.len() as u64
        || checkpoint.metadata.journal_sequence != recovered.sequence
        || (state.latest.is_some() && state.decision.is_none())
    {
        return Err(invalid());
    }
    let log = super::encode(1, &binding::record(instance)?).map_err(|_| invalid())?;
    let mut compacted = bytes.to_vec();
    compacted[HEADER + 128..HEADER + 136].copy_from_slice(&1_u64.to_le_bytes());
    compacted[HEADER + 136..HEADER + 144].copy_from_slice(&(log.len() as u64).to_le_bytes());
    compacted[HEADER + 144..HEADER + META]
        .copy_from_slice(checksum("gatel.checkpoint.journal-prefix.v1", &log).as_bytes());
    let sum = checksum("gatel.checkpoint.envelope.v2", &compacted[HEADER..]);
    compacted[8..HEADER].copy_from_slice(sum.as_bytes());
    let _ = decode(&compacted, instance, registry, &log)?;
    Ok((compacted, log))
}

/// Validate every complete transaction, even records after the checkpoint.
/// Returned state is staged locally and is discarded on any framing error.
fn log_state<'a>(bytes: &'a [u8], instance: &str) -> io::Result<(super::Recovered<'a>, State)> {
    let expected = binding::record(instance)?;
    let mut state = State::default();
    let recovered = recover_each(bytes, 0, |sequence, payload| {
        if sequence == 1 {
            if payload != expected {
                return Err(super::Error::InvalidBinding);
            }
            Ok(())
        } else {
            state.accept(sequence, payload)
        }
    })
    .map_err(|_| invalid())?;
    if recovered.sequence == 0 {
        return Err(invalid());
    }
    Ok((recovered, state))
}

fn plan_matches(sources: &Snapshot, registry: &Registry, expected: Fingerprint) -> io::Result<()> {
    let spec = normalize(sources, registry).map_err(|_| invalid())?;
    if compile(Arc::new(spec), registry).plan_hash() != expected {
        return Err(invalid());
    }
    Ok(())
}

/// Published contributions cannot disappear or move backwards in observed
/// intent. Equal revisions must retain every encoded field, including origins.
/// Do not normalize the observed union: rejected configuration is valid evidence.
fn source_history(sources: &Snapshot, observed: &Snapshot) -> io::Result<()> {
    sources.validate_successor(observed).map_err(|_| invalid())
}

/// Capture only at a quiescent, fully decided log boundary. This recompiles
/// outside transaction locks to check registry/plan compatibility, but does not
/// prepare resources or prove activation. Input log must be obtained under the
/// same writer coordination as the atomically captured configuration. The
/// capture can still include an in-flight observation; hosts must drain it first.
pub(super) fn encode<R>(
    instance: &str,
    registry: &Registry,
    log: &[u8],
    capture: &ConfigurationCapture<R>,
) -> io::Result<Vec<u8>> {
    let committed = capture.committed().ok_or_else(invalid)?;
    let (recovered, state) = log_state(log, instance)?;
    if recovered.incomplete_tail || (state.latest.is_some() && state.decision.is_none()) {
        return Err(invalid());
    }
    plan_matches(committed.source_snapshot(), registry, committed.plan_hash())?;
    let sources = snapshot_codec::encode(committed.source_snapshot()).map_err(|_| invalid())?;
    let observed = snapshot_codec::encode(capture.observed_snapshot()).map_err(|_| invalid())?;
    source_history(committed.source_snapshot(), capture.observed_snapshot())?;
    let mut publication = Hasher::new();
    publication.update_str("gatel.checkpoint.publication.v1");
    committed.boot_id.hash_into(&mut publication);
    publication.update_u64(committed.number.number());
    let instance = checksum(
        "gatel.checkpoint.instance.v1",
        binding::record(instance)?.as_bytes(),
    );
    let mut bytes = vec![0; HEADER];
    for fingerprint in [
        instance,
        registry.build_hash(),
        committed.plan_hash(),
        publication.finish(),
    ] {
        bytes.extend_from_slice(fingerprint.as_bytes());
    }
    bytes.extend_from_slice(&recovered.sequence.to_le_bytes());
    bytes.extend_from_slice(&(log.len() as u64).to_le_bytes());
    bytes.extend_from_slice(checksum("gatel.checkpoint.journal-prefix.v1", log).as_bytes());
    bytes.extend_from_slice(&(sources.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sources);
    bytes.extend_from_slice(&observed);
    let sum = checksum("gatel.checkpoint.envelope.v2", &bytes[HEADER..]);
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..HEADER].copy_from_slice(sum.as_bytes());
    Ok(bytes)
}

/// Check instance, registry, full journal framing/semantics, exact checkpoint
/// prefix and recompilation before exposing sources. Later valid pending intent
/// or a short tail remains the journal recovery layer's responsibility.
pub(super) fn decode(
    bytes: &[u8],
    instance: &str,
    registry: &Registry,
    log: &[u8],
) -> io::Result<Checkpoint> {
    if bytes.len() > MAX_BYTES
        || bytes.len() < HEADER + META + LENGTH
        || &bytes[..8] != MAGIC
        || checksum("gatel.checkpoint.envelope.v2", &bytes[HEADER..]).as_bytes() != &bytes[8..HEADER]
    {
        return Err(invalid());
    }
    let body = &bytes[HEADER..];
    let hash = |start| Fingerprint::from_bytes(body[start..start + 32].try_into().unwrap());
    let metadata = Metadata {
        instance: hash(0),
        build: hash(32),
        plan: hash(64),
        publication: hash(96),
        journal_sequence: u64::from_le_bytes(body[128..136].try_into().unwrap()),
        journal_bytes: u64::from_le_bytes(body[136..144].try_into().unwrap()),
        journal_prefix: hash(144),
    };
    let expected = checksum(
        "gatel.checkpoint.instance.v1",
        binding::record(instance)?.as_bytes(),
    );
    if metadata.instance != expected || metadata.build != registry.build_hash() {
        return Err(invalid());
    }
    // Inspect the entire supplied log, not only a valid older prefix that might
    // otherwise hide complete corruption in subsequent records.
    let _ = log_state(log, instance)?;
    let length = usize::try_from(metadata.journal_bytes).map_err(|_| invalid())?;
    let prefix = log.get(..length).ok_or_else(invalid)?;
    if checksum("gatel.checkpoint.journal-prefix.v1", prefix) != metadata.journal_prefix {
        return Err(invalid());
    }
    let (recovered, state) = log_state(prefix, instance)?;
    if recovered.incomplete_tail
        || recovered.sequence != metadata.journal_sequence
        || (state.latest.is_some() && state.decision.is_none())
    {
        return Err(invalid());
    }
    let source_length = usize::try_from(u64::from_le_bytes(body[META..META + LENGTH].try_into().unwrap()))
        .map_err(|_| invalid())?;
    let (source_bytes, observed_bytes) = body[META + LENGTH..]
        .split_at_checked(source_length)
        .ok_or_else(invalid)?;
    let sources = snapshot_codec::decode(source_bytes).map_err(|_| invalid())?;
    let observed = snapshot_codec::decode(observed_bytes).map_err(|_| invalid())?;
    source_history(&sources, &observed)?;
    plan_matches(&sources, registry, metadata.plan)?;
    Ok(Checkpoint {
        metadata,
        sources,
        observed,
    })
}

#[cfg(test)]
#[path = "admin_checkpoint_tests.rs"]
mod tests;
