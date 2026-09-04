//! Gatel's mutable runtime state.
//!
//! Everything here changes without the operator asking: a backend goes
//! unhealthy, a connection is reused, a balancer's cursor advances. None of it
//! is a configuration change, so none of it produces a new generation — and all
//! of it survives a compatible reload, because it is keyed by the resource that
//! owns it rather than by the generation that happened to create it.

#![forbid(unsafe_code)]

pub mod endpoint;
pub mod pool;
pub mod probe;
pub mod registry;
pub mod selector;
pub mod supervisor;
pub mod upstream;

pub use endpoint::{Endpoint, EndpointSet, Health, InFlight, probe_delay};
pub use pool::{Body, BoxError, CheckoutError, ConnectionPool, Lease, PoolStats};
pub use probe::probe;
pub use registry::{Resolution, StateKey, StateRegistry};
pub use selector::{RequestKey, Selection, SelectorState, select};
pub use supervisor::Supervisor;
pub use upstream::{UpstreamResolution, UpstreamRuntime, UpstreamRuntimes};
