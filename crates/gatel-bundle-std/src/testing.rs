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
