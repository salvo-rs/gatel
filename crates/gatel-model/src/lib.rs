//! Format-neutral configuration model for Gatel.
//!
//! This crate is deliberately dependency-free apart from the hash function: it
//! has no async runtime, no I/O and no HTTP types. Everything upstream of the
//! data plane — the KDL frontend, the Admin frontend, the compiler — agrees on
//! the types defined here.
//!
//! The four things it provides:
//!
//! - [`value`]: the configuration tree every frontend lowers into.
//! - [`decode`]: turning that tree into typed Rust values, strictly.
//! - [`diagnostic`]: what a rejection looks like.
//! - [`hash`], [`id`], [`secret`]: identity, fingerprints and secret handling.

#![forbid(unsafe_code)]

pub mod decode;
pub mod diagnostic;
pub mod hash;
pub mod id;
pub mod origin;
pub mod secret;
pub mod snapshot;
pub mod snapshot_codec;
mod snapshot_history;
pub use snapshot_history::SnapshotHistoryError;
pub mod value;
mod value_identity;

pub use decode::{ByteSize, FromNode, FromValue, NodeReader, parse_duration, parse_size, parse_socket_addr};
pub use diagnostic::{Code, Diagnostic, Diagnostics, Severity, did_you_mean};
pub use hash::{Fingerprint, Fingerprinted, Hasher};
pub use id::{BootId, Generation, ResourceId, ResourceKind, SourceId, SourceKind, SourceRev};
pub use origin::{Origin, Span};
pub use secret::{SecretRef, SecretString, resolve_secret};
pub use snapshot::{ObservationError, RevVector, Snapshot, SourceSnapshot, SourceUpdateError};
pub use value::{Node, Property, Scalar, Value};
