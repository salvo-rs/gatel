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
pub mod plan;
pub mod spec;
pub mod upstream;

pub use directive::{
    BuildContext, Bundle, Directive, DirectiveConfig, DirectiveInstance, DirectiveMeta, DirectiveState,
    FailurePolicy, Phase, Registry, RegistryBuilder, Role, Scope, Scopes,
};
pub use generation::{Committed, Failure, GenerationStore, Outcome, Preparation, Status};
pub use matcher::{HeaderLookup, HostMatch, MatchContext, Matcher, MethodMatch, PathMatch, parse_cidr};
pub use normalize::normalize;
pub use plan::{
    BindingChange, ListenerPlan, Plan, ResetPolicy, RoutePlan, SitePlan, StateBinding, StateKind, compile,
    diff_bindings,
};
pub use spec::{
    Admin, AdminBind, CertificateSource, ClientAuth, Global, Limits, Listener, ListenerProtocol, Log, Route,
    Site, SiteTls, Spec, Terminal,
};
pub use upstream::{
    Balance, Endpoint, Health, HealthKind, PassiveHealth, Pool, Retry, RetryMode, Timeouts, Upstream,
    UpstreamProtocol, UpstreamTls,
};
