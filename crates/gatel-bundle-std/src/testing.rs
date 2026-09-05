//! Helpers the directive tests share.
//!
//! Only compiled for tests. In a running process the telemetry handle is owned
//! by the process and reaches a directive through `BuildContext`; there is
//! deliberately no public way to reach one from anywhere else.

use std::sync::{Arc, OnceLock};

use gatel_observe::Telemetry;

/// A handle that lives as long as the test binary, so a `BuildContext` can
/// borrow it without every test owning one.
pub fn telemetry() -> &'static Arc<Telemetry> {
    static TELEMETRY: OnceLock<Arc<Telemetry>> = OnceLock::new();
    TELEMETRY.get_or_init(Telemetry::new)
}

/// An `Upstreams` with nothing in it, for the directives that never ask one
/// anything — which is most of them.
///
/// A directive that does ask gets a real one from a test that sets a backend
/// up; there is deliberately no fake that answers, because a fake verdict
/// would prove nothing about the path a verdict actually takes.
pub fn no_upstreams() -> &'static gatel_plan::SharedUpstreams {
    static UPSTREAMS: OnceLock<gatel_plan::SharedUpstreams> = OnceLock::new();
    UPSTREAMS.get_or_init(|| Arc::new(gatel_plan::NoUpstreams))
}
