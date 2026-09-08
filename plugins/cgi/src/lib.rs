//! Process CGI exchange, generation-owned supervision, and platform process
//! primitives. The optional HTTP bundle supports Windows; Linux whole-scope
//! invocation completion remains pending and its configuration is rejected.
#![forbid(unsafe_code)]
mod adapter;
pub mod exchange;
#[cfg(target_os = "linux")]
pub mod process;
#[cfg(windows)]
#[path = "process_windows.rs"]
pub mod process;
pub mod supervisor;
pub use adapter::bundle;
#[cfg(windows)]
pub mod job;
