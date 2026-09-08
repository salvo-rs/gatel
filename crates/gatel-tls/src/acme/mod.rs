//! Automatic certificates.
//!
//! The ACME protocol itself is `certon`'s: the account, the order, the
//! challenge exchange and the CSR are its business. What lives here is the part
//! that is Gatel's — where the answer to a challenge is served, where a
//! certificate ends up, and when it is renewed — because each of those already
//! has an owner in this design and adding a second one would break an
//! invariant.
//!
//! - The challenge is answered by the listener the configuration declares, not by a server the library
//!   starts. See [`challenge`].
//! - A certificate goes into the same [`CertificateStore`] as every other one, so a renewal replaces it while
//!   established connections carry on and no generation is produced.
//! - Renewal is a background task reconciled after a commit, like a health check, so a candidate that fails
//!   to prepare starts nothing.

mod account_lock;
mod account_pair;
mod account_storage;
mod cert_store;
pub mod challenge;
mod identity_file;
pub mod issuer;
pub mod manager;
pub(crate) mod order;
mod registration_intent;

pub use cert_store::AtomicCertStore;
pub use certon::CertStore;
pub use certon::storage::CertificateResource;
pub use challenge::{Challenges, RouteSolver};
pub use issuer::{Issued, Issuer};
pub use manager::{Acme, Wanted};
