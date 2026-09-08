//! Optional SCGI transport and HTTP terminal.
#![forbid(unsafe_code)]
mod adapter;
pub mod exchange;
pub use adapter::bundle;
