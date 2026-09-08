//! Platform-only FFI boundary. No HTTP, configuration, spawning or async work.
//! Job handles are private, non-inherited and closed exactly once by OwnedHandle.
#![deny(unsafe_op_in_unsafe_fn)]
#[cfg(windows)]
mod job;
#[cfg(windows)]
pub use job::Job;
