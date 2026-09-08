//! HTTP mapping and lifecycle shared by CGI transports, not a connection pool.
#![forbid(unsafe_code)]
mod backend;
mod input;
mod mapping;
mod terminal;
pub use backend::{Backend, BackendConfig, BackendFactory, Invocation};
pub use terminal::{
    ExchangeError, ExchangeRequest, Protocol, Running, Sink, Transport, backend_directive, directive,
};
