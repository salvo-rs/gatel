//! Gatel's HTTP data plane.
//!
//! Sockets belong to the process, route tables belong to a generation, and a
//! connection belongs to the generation that was current when it was accepted.
//! Keeping those three lifetimes apart is what makes a reload invisible to
//! traffic: the socket never closes, the new table is published atomically, and
//! nothing in flight changes underneath a request.

#![forbid(unsafe_code)]

pub mod bind;
pub mod dispatch;
pub mod prepare;
pub mod proxy;
pub mod serve;

pub use bind::BindTable;
pub use dispatch::{Dispatcher, NoRoute, PreparedRoute, PreparedSite, request_host};
pub use prepare::{ListenerRuntime, Runtime, prepare};
pub use proxy::ProxyTerminal;
pub use serve::{Server, Store};
