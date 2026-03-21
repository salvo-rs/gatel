//! TLS certificate management for gatel.
//!
//! This module provides [`TlsManager`], which integrates with `certon` for
//! automatic ACME certificate issuance and supports manually-specified PEM
//! certificates on a per-site basis.

pub mod dns;
pub mod manager;

pub use manager::TlsManager;
