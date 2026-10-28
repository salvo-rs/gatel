//! Gatel's compiler and configuration transaction.
//!
//! The pipeline is one directed path, and everything else in the project sits
//! on one side of it or the other:
//!
//! ```text
//! Snapshot ──normalize──> Spec ──compile──> Plan ──prepare──> Generation
//!            (resolve,             (order,          (bind,      (published
//!             validate)             bind state)      build)      atomically)
//! ```
//!
//! `Spec` answers "what was asked for", `Plan` answers "how it will behave",
//! and their two fingerprints are what let an unchanged reload stop early and
//! a differently-built binary refuse to reuse a plan that is not its own.

#![forbid(unsafe_code)]

pub mod directive;
pub mod generation;
pub mod matcher;
pub mod normalize;
pub mod ordering;
pub mod plan;
pub mod publication;
mod request_path;
pub mod spec;
pub mod subrequest;
pub mod upstream;

pub use directive::{
    BuildContext, Bundle, Directive, DirectiveConfig, DirectiveInstance, DirectiveMeta, DirectiveState,
    Effect, FailurePolicy, Phase, Registry, RegistryBuilder, Role, Scope, Scopes,
};
pub use gatel_plugin_runtime::secrets::SecretDependencies;
pub use gatel_plugin_runtime::{
    BackgroundTask, BodyTooLarge, ExecutionIdentity, PreparedDirective, RequestBodyLimit, Shutdown,
    ShutdownSignal, blocking,
};
pub use generation::{
    Committed, ConfigurationCapture, Conflict, Failure, GenerationStore, Outcome, Preparation,
    RestoreIntentError, SourceWriteError, SourceWriter, Status, Version,
};
pub use matcher::{HeaderLookup, HostMatch, MatchContext, Matcher, MethodMatch, PathMatch, parse_cidr};
pub use normalize::normalize;
pub use plan::{
    BindingChange, ListenerPlan, Plan, ResetPolicy, RoutePlan, SitePlan, StateBinding, StateKind, compile,
    diff_bindings,
};
pub use request_path::routing_path;
pub use spec::{
    Acme, CertificateSource, ClientAuth, Global, Limits, Listener, ListenerProtocol, Log, Route, Site,
    SiteTls, Spec, Terminal,
};
pub use subrequest::{
    Answer, Ask, HeaderMap, Method, NoUpstreams, ScopedUpstreams, SharedUpstreams, StatusCode,
    UpstreamHandle, Upstreams, async_trait,
};
pub use upstream::{
    Balance, Endpoint, Family, Health, HealthKind, PassiveHealth, Pool, Resolve, Retry, RetryMode, Target,
    Timeouts, Upstream, UpstreamProtocol, UpstreamTls,
};
