//! TLS for Gatel.
//!
//! Certificates live in state rather than in configuration, because they change
//! on their own schedule: a renewal is not a configuration change and must not
//! produce a generation. Configuration says *where* a certificate comes from;
//! this crate holds the certificate itself and hands it to each handshake.

#![forbid(unsafe_code)]

pub mod acme;
pub mod certificate;
pub mod internal;
pub mod server;
pub mod store;

pub use acme::{Acme, Challenges, Issued, Issuer, RouteSolver, Wanted};
pub use certificate::Certificate;
pub use internal::InternalCa;
/// Re-exported so nothing above this crate has to name rustls directly.
pub use rustls::ServerConfig;
pub use server::{TlsPreparation, prepare_listener};
pub use store::{CertificateStore, SniResolver};
pub use tokio_rustls::TlsAcceptor;
