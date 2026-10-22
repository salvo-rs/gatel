//! Gatel's HTTP data plane.
//!
//! Sockets belong to the process, route tables belong to a generation, and a
//! connection belongs to the generation that was current when it was accepted.
//! Keeping those three lifetimes apart is what makes a reload invisible to
//! traffic: the socket never closes, the new table is published atomically, and
//! nothing in flight changes underneath a request.

#![forbid(unsafe_code)]

pub mod bind;
pub mod body;
mod connection_drain;
mod connection_work;
pub mod dispatch;
mod execution;
pub mod measure;
mod plugin_prepare;
pub mod plugin_retirement;
pub mod plugin_tasks;
pub mod prepare;
pub mod proxy;
pub mod proxy_protocol;
pub mod serve;
pub mod upgrade;

pub use bind::BindTable;
pub use dispatch::{Dispatcher, NoRoute, PreparedRoute, PreparedSite, request_host};
pub use prepare::{ListenerRuntime, Resources, Runtime, prepare};
pub use proxy::ProxyTerminal;
pub use serve::{Server, Store};
pub use upgrade::{Ended, requested_protocol};
