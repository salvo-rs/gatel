//! TLS certificate management for gatel.
//!
//! This module provides [`TlsManager`], which integrates with `certon` for
//! automatic ACME certificate issuance and supports manually-specified PEM
//! certificates on a per-site basis.

pub mod dns;
pub mod local_ca;
pub mod manager;
pub mod trust_store;

pub use local_ca::{LocalCa, default_storage_dir as default_local_ca_dir};
pub use manager::TlsManager;
