//! Binary WAL framing only. No disk I/O, source decoding or transaction replay.
use super::{checksum, wal_intent};

const MAGIC: &[u8; 8] = b"GATELW01";
pub(super) const HEADER: usize = 96;
pub(super) const MAX_LOG: usize = 64 * 1024 * 1024;
pub(super) const DECISION_BYTES: usize = 41;
pub(super) const DECISION_RESERVE: usize = HEADER + DECISION_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Intent,
    Decision,
}
impl Kind {
    fn tag(self) -> u8 {
        match self {
            Self::Intent => 1,
            Self::Decision => 2,
        }
    }
    fn validate(self, length: usize) -> Result<(), Error> {
        match self {
            Self::Intent if length > 0 && length <= wal_intent::MAX_BYTES => Ok(()),
            Self::Decision if length == DECISION_BYTES => Ok(()),
            _ => Err(Error::Length),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Limit,
    Header,
    Payload,
    Length,
    Sequence,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "binary WAL capacity exceeded",
            Self::Header => "invalid binary WAL header",
            Self::Payload => "invalid binary WAL payload checksum",
            Self::Length => "invalid binary WAL payload length",
            Self::Sequence => "invalid or exhausted binary WAL sequence",
        })
    }
}
impl std::error::Error for Error {}

pub(super) struct Frame<'a> {
    pub sequence: u64,
    pub kind: Kind,
    pub payload: &'a [u8],
}
impl std::fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalFrame")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Scan {
    pub sequence: u64,
    pub valid_bytes: usize,
    pub incomplete_tail: bool,
}

/// Reserve both the intent and its fixed-size decision, including two sequence
/// numbers, before any append. This cannot guarantee physical disk capacity.
pub(super) fn reserve_intent(used: usize, sequence: u64, payload_bytes: usize) -> Result<(), Error> {
    Kind::Intent.validate(payload_bytes)?;
    sequence.checked_add(2).ok_or(Error::Sequence)?;
    let required = used
        .checked_add(HEADER)
        .and_then(|n| n.checked_add(payload_bytes))
        .and_then(|n| n.checked_add(DECISION_RESERVE))
        .ok_or(Error::Limit)?;
    if required > MAX_LOG {
        return Err(Error::Limit);
    }
    Ok(())
}

pub(super) fn encode(sequence: u64, kind: Kind, payload: &[u8]) -> Result<Vec<u8>, Error> {
    if sequence == 0 {
        return Err(Error::Sequence);
    }
    kind.validate(payload.len())?;
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.push(kind.tag());
    bytes.extend_from_slice(&[0; 7]);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(checksum("gatel.wal.frame-payload.v1", payload).as_bytes());
    let header = checksum("gatel.wal.frame-header.v1", &bytes);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Base must come from a separately validated checkpoint, never the first frame.
/// A partial final frame is reported, not removed or treated as a decision.
/// Visitors must stage changes locally and discard them on any later error;
/// framing does not validate intent semantics or intent/decision transitions.
pub(super) fn scan<'a>(
    input: &'a [u8],
    base: u64,
    mut visit: impl FnMut(Frame<'a>) -> Result<(), Error>,
) -> Result<Scan, Error> {
    if input.len() > MAX_LOG {
        return Err(Error::Limit);
    }
    let mut result = Scan {
        sequence: base,
        valid_bytes: 0,
        incomplete_tail: false,
    };
    while result.valid_bytes < input.len() {
        let remaining = &input[result.valid_bytes..];
        let prefix = remaining.len().min(MAGIC.len());
        if remaining[..prefix] != MAGIC[..prefix] {
            return Err(Error::Header);
        }
        if remaining.len() < HEADER {
            result.incomplete_tail = true;
            break;
        }
        // Authenticate the length against accidental corruption before using it
        // to distinguish a truncated write from a fully present bad record.
        if checksum("gatel.wal.frame-header.v1", &remaining[..64]).as_bytes() != &remaining[64..HEADER]
            || remaining[17..24] != [0; 7]
        {
            return Err(Error::Header);
        }
        let kind = match remaining[16] {
            1 => Kind::Intent,
            2 => Kind::Decision,
            _ => return Err(Error::Header),
        };
        let sequence = u64::from_le_bytes(remaining[8..16].try_into().unwrap());
        if sequence != result.sequence.checked_add(1).ok_or(Error::Sequence)? {
            return Err(Error::Sequence);
        }
        let length = usize::try_from(u64::from_le_bytes(remaining[24..32].try_into().unwrap()))
            .map_err(|_| Error::Length)?;
        kind.validate(length)?;
        if remaining.len() - HEADER < length {
            result.incomplete_tail = true;
            break;
        }
        let payload = &remaining[HEADER..HEADER + length];
        if checksum("gatel.wal.frame-payload.v1", payload).as_bytes() != &remaining[32..64] {
            return Err(Error::Payload);
        }
        visit(Frame {
            sequence,
            kind,
            payload,
        })?;
        result.sequence = sequence;
        result.valid_bytes += HEADER + length;
    }
    Ok(result)
}

#[cfg(test)]
#[path = "admin_wal_frame_tests.rs"]
mod tests;
