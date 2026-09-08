//! Host decisions reference an exact prior intent; they are not HTTP receipts.
use gatel_model::Fingerprint;

use super::wal_frame::{DECISION_BYTES, Error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    Rejected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Decision {
    pub intent_sequence: u64,
    pub intent_identity: Fingerprint,
    pub outcome: Outcome,
}
impl std::fmt::Debug for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalDecision")
            .field("intent_sequence", &self.intent_sequence)
            .field("outcome", &self.outcome)
            .finish()
    }
}
impl Decision {
    pub(super) fn encode(self) -> Result<[u8; DECISION_BYTES], Error> {
        if self.intent_sequence == 0 {
            return Err(Error::Sequence);
        }
        let mut bytes = [0; DECISION_BYTES];
        bytes[..8].copy_from_slice(&self.intent_sequence.to_le_bytes());
        bytes[8..40].copy_from_slice(self.intent_identity.as_bytes());
        bytes[40] = match self.outcome {
            Outcome::Applied => 1,
            Outcome::Rejected => 2,
        };
        Ok(bytes)
    }

    /// Matching identity and pending-transaction state are checked by replay.
    /// A canceled caller or uncertain I/O must never manufacture Rejected.
    pub(super) fn decode(bytes: &[u8], decision_sequence: u64) -> Result<Self, Error> {
        if bytes.len() != DECISION_BYTES {
            return Err(Error::Length);
        }
        let intent_sequence = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        if intent_sequence == 0 || intent_sequence.checked_add(1) != Some(decision_sequence) {
            return Err(Error::Sequence);
        }
        let outcome = match bytes[40] {
            1 => Outcome::Applied,
            2 => Outcome::Rejected,
            _ => return Err(Error::Payload),
        };
        Ok(Self {
            intent_sequence,
            intent_identity: Fingerprint::from_bytes(bytes[8..40].try_into().unwrap()),
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_roundtrip_but_unknown_outcomes_and_invalid_references_fail() {
        let identity = crate::admin::journal::checksum("fixture", b"private");
        for outcome in [Outcome::Applied, Outcome::Rejected] {
            let decision = Decision {
                intent_sequence: 7,
                intent_identity: identity,
                outcome,
            };
            let bytes = decision.encode().unwrap();
            assert_eq!(Decision::decode(&bytes, 8).unwrap(), decision);
            for sequence in [0, 6, 7, 9, u64::MAX] {
                assert_eq!(Decision::decode(&bytes, sequence), Err(Error::Sequence));
            }
            for cut in 0..bytes.len() {
                assert_eq!(Decision::decode(&bytes[..cut], 8), Err(Error::Length));
            }
            let mut unknown = bytes;
            unknown[40] = 0;
            assert_eq!(Decision::decode(&unknown, 8), Err(Error::Payload));
            assert!(!format!("{decision:?}").contains("private"));
        }
        let zero = Decision {
            intent_sequence: 0,
            intent_identity: identity,
            outcome: Outcome::Applied,
        };
        assert_eq!(zero.encode(), Err(Error::Sequence));
        assert_eq!(Decision::decode(&[0; DECISION_BYTES], 1), Err(Error::Sequence));
        let exhausted = Decision {
            intent_sequence: u64::MAX,
            ..zero
        }
        .encode()
        .unwrap();
        assert_eq!(Decision::decode(&exhausted, 0), Err(Error::Sequence));
    }
}
