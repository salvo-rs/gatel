//! TLS for Gatel.
//!
//! Certificates live in state rather than in configuration. Automatic renewals
//! update that state without a generation; manual file replacements are
//! observed by the host's conditional runtime refresh. Configuration says
//! *where* a certificate comes from; this crate holds the certificate itself
//! and hands it to each handshake.

#![forbid(unsafe_code)]

pub mod acme;
pub mod certificate;
pub mod client;
pub mod internal;
pub mod server;
mod server_material;
pub mod store;

pub use acme::{Acme, Challenges, Issued, Issuer, RouteSolver, Wanted};
pub use certificate::Certificate;
pub use client::ClientTls;
pub use internal::InternalCa;
/// Re-exported so nothing above this crate has to name rustls directly.
pub use rustls::ServerConfig;
pub use server::{TlsPreparation, prepare_listener, prepare_listener_observed};
pub use server_material::ServerMaterial;
pub use store::{CertificateStore, SniResolver};
pub use tokio_rustls::TlsAcceptor;
