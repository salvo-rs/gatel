//! Versioned admin-intent framing and append storage. No commit integration yet.
#![allow(
    dead_code,
    reason = "Journal storage is not yet connected to configuration transactions"
)]
use gatel_model::{Fingerprint, Hasher};

#[path = "admin_journal_store.rs"]
mod store;

#[path = "admin_journal_transaction.rs"]
mod transaction;

#[path = "admin_journal_binding.rs"]
mod binding;

#[path = "admin_checkpoint.rs"]
mod checkpoint;

// Complete-source WAL payloads are staged separately from the legacy text log.
#[path = "admin_wal_intent.rs"]
mod wal_intent;

#[path = "admin_wal_frame.rs"]
mod wal_frame;

#[path = "admin_wal_decision.rs"]
mod wal_decision;

#[path = "admin_checkpoint_writer.rs"]
pub(crate) mod checkpoint_writer;

#[path = "admin_journal_cli.rs"]
pub(crate) mod cli;

#[path = "admin_bootstrap.rs"]
pub(crate) mod bootstrap;

const MAGIC: &[u8; 8] = b"GATELJ01";
const HEADER: usize = 84;
const MAX_LOG: usize = 16 * 1024 * 1024;
// Transaction envelope, in addition to the full HTTP body allowance.
const MAX_PAYLOAD: usize = super::MAX_BODY + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    TooLarge,
    InvalidMagic,
    InvalidHeader,
    InvalidLength,
    InvalidPayload,
    InvalidUtf8,
    InvalidSequence,
    SequenceExhausted,
    InvalidRecord,
    InvalidBinding,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "admin journal exceeds its size limit",
            Self::InvalidMagic => "unsupported admin journal format",
            Self::InvalidHeader => "admin journal header checksum mismatch",
            Self::InvalidLength => "invalid admin journal payload length",
            Self::InvalidPayload => "admin journal payload checksum mismatch",
            Self::InvalidUtf8 => "admin journal payload is not UTF-8",
            Self::InvalidSequence => "admin journal sequence is not contiguous",
            Self::SequenceExhausted => "admin journal sequence is exhausted",
            Self::InvalidRecord => "invalid admin journal transaction record or transition",
            Self::InvalidBinding => "missing, invalid or mismatched admin journal instance binding",
        })
    }
}
impl std::error::Error for Error {}

pub(super) struct Recovered<'a> {
    pub sequence: u64,
    /// None means no complete record; Some("") is an explicit empty intent.
    pub text: Option<&'a str>,
    pub valid_bytes: usize,
    pub incomplete_tail: bool,
}
impl std::fmt::Debug for Recovered<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recovered")
            .field("sequence", &self.sequence)
            .field("text_bytes", &self.text.map(str::len))
            .field("valid_bytes", &self.valid_bytes)
            .field("incomplete_tail", &self.incomplete_tail)
            .finish()
    }
}

fn checksum(domain: &str, bytes: &[u8]) -> Fingerprint {
    let mut hash = Hasher::new();
    hash.update_str(domain).update_bytes(bytes);
    hash.finish()
}

pub(super) fn encode(sequence: u64, text: &str) -> Result<Vec<u8>, Error> {
    if sequence == 0 {
        return Err(Error::InvalidSequence);
    }
    if text.len() > MAX_PAYLOAD {
        return Err(Error::TooLarge);
    }
    let mut bytes = Vec::with_capacity(HEADER + text.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
    bytes.extend_from_slice(checksum("gatel.admin-intent.payload.v1", text.as_bytes()).as_bytes());
    let header = checksum("gatel.admin-intent.header.v1", &bytes);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

/// `base` must come from a separately validated checkpoint, or zero for a new
/// log. This does not discover or trust a base sequence from the first record.
/// Truncated trailing bytes are reported, never automatically deleted here.
pub(super) fn recover(input: &[u8], base: u64) -> Result<Recovered<'_>, Error> {
    recover_each(input, base, |_, _| Ok(()))
}

/// Visitors must stage their state locally and discard it on any error: a
/// later record can invalidate the whole scan after earlier visits succeeded.
fn recover_each<'a>(
    input: &'a [u8],
    base: u64,
    mut visit: impl FnMut(u64, &'a str) -> Result<(), Error>,
) -> Result<Recovered<'a>, Error> {
    if input.len() > MAX_LOG {
        return Err(Error::TooLarge);
    }
    let mut result = Recovered {
        sequence: base,
        text: None,
        valid_bytes: 0,
        incomplete_tail: false,
    };
    while result.valid_bytes < input.len() {
        let remaining = &input[result.valid_bytes..];
        let prefix = remaining.len().min(MAGIC.len());
        if remaining[..prefix] != MAGIC[..prefix] {
            return Err(Error::InvalidMagic);
        }
        if remaining.len() < HEADER {
            result.incomplete_tail = true;
            break;
        }
        // Validate the length-bearing header before trusting its length. A bit
        // flip must not turn an intact corrupt record into an ignored short tail.
        if checksum("gatel.admin-intent.header.v1", &remaining[..52]).as_bytes() != &remaining[52..HEADER] {
            return Err(Error::InvalidHeader);
        }
        let sequence = u64::from_le_bytes(remaining[8..16].try_into().unwrap());
        let expected = result.sequence.checked_add(1).ok_or(Error::SequenceExhausted)?;
        if sequence != expected {
            return Err(Error::InvalidSequence);
        }
        let length = u32::from_le_bytes(remaining[16..20].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD {
            return Err(Error::InvalidLength);
        }
        if remaining.len() - HEADER < length {
            result.incomplete_tail = true;
            break;
        }
        let payload = &remaining[HEADER..HEADER + length];
        if checksum("gatel.admin-intent.payload.v1", payload).as_bytes() != &remaining[20..52] {
            return Err(Error::InvalidPayload);
        }
        let text = std::str::from_utf8(payload).map_err(|_| Error::InvalidUtf8)?;
        visit(sequence, text)?;
        result.sequence = sequence;
        result.text = Some(text);
        result.valid_bytes += HEADER + length;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_records_recover_latest_intent_and_distinguish_tombstones() {
        let empty = recover(&[], 0).unwrap();
        assert_eq!(empty.text, None);
        let first = encode(1, "upstream \"runtime/first\";").unwrap();
        let mut log = first.clone();
        log.extend(encode(2, "").unwrap());
        let recovered = recover(&log, 0).unwrap();
        assert_eq!(recovered.sequence, 2);
        assert_eq!(recovered.text, Some(""));
        assert_eq!(recovered.valid_bytes, log.len());
        assert!(!recovered.incomplete_tail);
        assert!(!format!("{:?}", recover(&first, 0).unwrap()).contains("runtime/first"));
    }

    #[test]
    fn every_partial_append_preserves_only_the_complete_prefix() {
        let first = encode(1, "first;").unwrap();
        let second = encode(2, "next \"配置\";").unwrap();
        for cut in 0..second.len() {
            let mut log = first.clone();
            log.extend_from_slice(&second[..cut]);
            let recovered = recover(&log, 0).unwrap();
            assert_eq!(recovered.text, Some("first;"), "cut={cut}");
            assert_eq!(recovered.sequence, 1);
            assert_eq!(recovered.valid_bytes, first.len());
            assert_eq!(recovered.incomplete_tail, cut != 0);
        }
    }

    #[test]
    fn intact_corruption_is_not_a_truncated_tail_or_a_partial_success() {
        let first = encode(1, "first;").unwrap();
        let second = encode(2, "secret-payload").unwrap();
        for offset in 0..second.len() {
            let mut changed = second.clone();
            changed[offset] ^= 1;
            let mut log = first.clone();
            log.extend(changed);
            assert!(recover(&log, 0).is_err(), "corruption offset={offset}");
        }
        let mut log = first;
        log.extend_from_slice(b"bad-tail");
        assert_eq!(recover(&log, 0).unwrap_err(), Error::InvalidMagic);
    }

    #[test]
    fn gaps_duplicates_rollback_and_exhaustion_are_rejected() {
        for sequence in [1, 3] {
            let mut log = encode(1, "first").unwrap();
            log.extend(encode(sequence, "bad").unwrap());
            assert_eq!(recover(&log, 0).unwrap_err(), Error::InvalidSequence);
        }
        assert_eq!(encode(0, "bad").unwrap_err(), Error::InvalidSequence);
        assert_eq!(
            recover(&encode(9, "bad").unwrap(), 0).unwrap_err(),
            Error::InvalidSequence
        );
        let last = encode(u64::MAX, "last").unwrap();
        assert_eq!(recover(&last, u64::MAX - 1).unwrap().sequence, u64::MAX);
        assert_eq!(recover(&last, u64::MAX).unwrap_err(), Error::SequenceExhausted);
    }

    #[test]
    fn byte_limits_and_checksummed_non_utf8_are_rejected() {
        let max = "x".repeat(MAX_PAYLOAD);
        assert_eq!(
            recover(&encode(1, &max).unwrap(), 0).unwrap().text.unwrap().len(),
            max.len()
        );
        assert_eq!(encode(1, &(max + "x")).unwrap_err(), Error::TooLarge);
        assert_eq!(recover(&vec![0; MAX_LOG + 1], 0).unwrap_err(), Error::TooLarge);
        let mut record = encode(1, "x").unwrap();
        record[HEADER] = 255;
        let payload = checksum("gatel.admin-intent.payload.v1", &record[HEADER..]);
        record[20..52].copy_from_slice(payload.as_bytes());
        let header = checksum("gatel.admin-intent.header.v1", &record[..52]);
        record[52..HEADER].copy_from_slice(header.as_bytes());
        assert_eq!(recover(&record, 0).unwrap_err(), Error::InvalidUtf8);
        record[16..20].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        let header = checksum("gatel.admin-intent.header.v1", &record[..52]);
        record[52..HEADER].copy_from_slice(header.as_bytes());
        assert_eq!(recover(&record, 0).unwrap_err(), Error::InvalidLength);
    }
}
