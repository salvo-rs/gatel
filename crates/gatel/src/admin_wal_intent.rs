//! Complete-source intent payload, not yet a production WAL storage protocol.
//! No I/O, source authorization, replay decision or durability receipt here.
use gatel_model::{BootId, Fingerprint, Hasher, Snapshot, SourceId, snapshot_codec};
use gatel_plan::Version;

use super::{binding, checksum};

const MAGIC: &[u8; 8] = b"GATELI01";
const HEADER: usize = 40;
const META: usize = 32 * 5;
const LENGTH: usize = 8;
pub(super) const MAX_BYTES: usize = HEADER + META + LENGTH + snapshot_codec::MAX_BYTES;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestId(Fingerprint);
impl std::fmt::Debug for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestId(..)")
    }
}
impl RequestId {
    /// Host authorizes the namespace separately. Keys are opaque and case-sensitive.
    pub(super) fn client(source: &SourceId, key: &str) -> Result<Self, Error> {
        if key.is_empty() || key.len() > 128 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Error::InvalidKey);
        }
        let mut hash = Hasher::new();
        hash.update_str("gatel.wal.client-request.v1");
        hash.update_str(&source.to_string());
        hash.update_str(key);
        Ok(Self(hash.finish()))
    }

    /// File observations can receive an internal identity without a client key.
    pub(super) fn generate() -> Self {
        let mut hash = Hasher::new();
        hash.update_str("gatel.wal.host-request.v1");
        BootId::generate().hash_into(&mut hash);
        Self(hash.finish())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    InvalidKey,
    Invalid,
    Limit,
    Context,
    History,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidKey => "WAL request key must be 1..128 visible ASCII bytes",
            Self::Invalid => "invalid WAL intent payload",
            Self::Limit => "WAL intent payload exceeds limits",
            Self::Context => "WAL intent context does not match",
            Self::History => "WAL intent violates source history",
        })
    }
}
impl std::error::Error for Error {}

pub(super) struct Intent {
    pub request: RequestId,
    /// Opaque identity of the original precondition, not a new boot's CAS token.
    pub base_version: Fingerprint,
    /// Hash of the entire envelope body; same key with different payload conflicts.
    pub identity: Fingerprint,
    pub sources: Snapshot,
}
impl std::fmt::Debug for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalIntent")
            .field("source_count", &self.sources.sources().count())
            .finish()
    }
}

fn instance_hash(instance: &str) -> Result<Fingerprint, Error> {
    let binding = binding::record(instance).map_err(|_| Error::Context)?;
    Ok(checksum("gatel.wal.intent-instance.v1", binding.as_bytes()))
}
fn source_bytes(snapshot: &Snapshot) -> Result<Vec<u8>, Error> {
    snapshot_codec::encode(snapshot).map_err(codec_error)
}
fn codec_error(error: snapshot_codec::Error) -> Error {
    match error {
        snapshot_codec::Error::Limit => Error::Limit,
        snapshot_codec::Error::Invalid => Error::Invalid,
    }
}

/// Caller must capture base and version atomically and serialize all sources.
/// Base is retained externally (checkpoint/earlier decided intent); the payload
/// pins its exact encoded identity, not only its revision vector or effective plan.
pub(super) fn encode(
    instance: &str,
    build: Fingerprint,
    request: RequestId,
    version: &Version,
    base: &Snapshot,
    candidate: &Snapshot,
) -> Result<Vec<u8>, Error> {
    let instance = instance_hash(instance)?;
    if version.revs != base.revs() {
        return Err(Error::Context);
    }
    base.validate_successor(candidate).map_err(|_| Error::History)?;
    let base = source_bytes(base)?;
    let sources = source_bytes(candidate)?;
    let mut bytes = Vec::with_capacity(HEADER + META + LENGTH + sources.len());
    bytes.resize(HEADER, 0);
    for value in [
        instance,
        build,
        request.0,
        checksum("gatel.wal.base-version.v1", version.etag().as_bytes()),
        checksum("gatel.wal.base-sources.v1", &base),
    ] {
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&(sources.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sources);
    let identity = checksum("gatel.wal.intent-envelope.v1", &bytes[HEADER..]);
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..HEADER].copy_from_slice(identity.as_bytes());
    Ok(bytes)
}

/// Bounded decoding requires the exact retained base, deployment and build.
/// Latest intent is not compiled: semantic rejection must remain representable.
/// Hashes detect corruption/context mismatch, not a malicious storage owner.
pub(super) fn decode(
    bytes: &[u8],
    instance: &str,
    build: Fingerprint,
    base: &Snapshot,
) -> Result<Intent, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    if bytes.len() < HEADER + META + LENGTH || &bytes[..8] != MAGIC {
        return Err(Error::Invalid);
    }
    let identity = checksum("gatel.wal.intent-envelope.v1", &bytes[HEADER..]);
    if identity.as_bytes() != &bytes[8..HEADER] {
        return Err(Error::Invalid);
    }
    let hash =
        |offset| Fingerprint::from_bytes(bytes[HEADER + offset..HEADER + offset + 32].try_into().unwrap());
    if hash(0) != instance_hash(instance)?
        || hash(32) != build
        || hash(128) != checksum("gatel.wal.base-sources.v1", &source_bytes(base)?)
    {
        return Err(Error::Context);
    }
    let length = u64::from_le_bytes(bytes[HEADER + META..HEADER + META + LENGTH].try_into().unwrap());
    let payload = &bytes[HEADER + META + LENGTH..];
    if length != payload.len() as u64 {
        return Err(Error::Invalid);
    }
    let sources = snapshot_codec::decode(payload).map_err(codec_error)?;
    base.validate_successor(&sources).map_err(|_| Error::History)?;
    Ok(Intent {
        request: RequestId(hash(64)),
        base_version: hash(96),
        identity,
        sources,
    })
}

#[cfg(test)]
#[path = "admin_wal_intent_tests.rs"]
mod tests;
