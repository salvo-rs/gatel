//! FastCGI transport. The HTTP terminal/configuration adapter is implemented
//! separately; this exchange owns no detached tasks or shared HTTP pool slots.
#![forbid(unsafe_code)]
mod adapter;
pub mod exchange;
pub use adapter::bundle;
