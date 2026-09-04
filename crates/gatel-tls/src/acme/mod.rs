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

pub mod challenge;
pub mod issuer;
pub mod manager;

pub use challenge::{Challenges, RouteSolver};
pub use issuer::{Issued, Issuer};
pub use manager::{Acme, Wanted};
