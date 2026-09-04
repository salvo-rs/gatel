//! The standard directive bundle.
//!
//! Each directive here is three small pieces: a struct with
//! `#[derive(FromNode)]` for its configuration, a `DirectiveConfig` impl that
//! says how it hashes and what handler it becomes, and a `Directive` impl
//! carrying its metadata. Nothing in a directive decides its own position in
//! the chain — the phase in its metadata does.

#![forbid(unsafe_code)]

pub mod access_log;
pub mod headers;
pub mod redirect;
pub mod respond;

use gatel_plan::Bundle;

/// Every directive this crate provides.
pub fn bundle() -> Bundle {
    Bundle::new("std", "1")
        .with(respond::Respond)
        .with(redirect::Redirect)
        .with(headers::Headers)
        .with(access_log::AccessLog)
}

#[cfg(test)]
mod tests {
    use gatel_plan::{Registry, Role};

    use super::*;

    #[test]
    fn the_bundle_builds_and_claims_distinct_names() {
        let registry = Registry::builder().bundle(bundle()).build().unwrap();
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, ["access-log", "headers", "redirect", "respond"]);
    }

    #[test]
    fn exactly_the_answering_directives_are_terminals() {
        let registry = Registry::builder().bundle(bundle()).build().unwrap();
        let terminals: Vec<&str> = registry
            .names()
            .filter(|name| registry.get(name).unwrap().meta().role == Role::Terminal)
            .collect();
        assert_eq!(terminals, ["redirect", "respond"]);
    }

    #[test]
    fn only_observation_may_fail_open() {
        let registry = Registry::builder().bundle(bundle()).build().unwrap();
        let open: Vec<&str> = registry
            .names()
            .filter(|name| {
                registry.get(name).unwrap().meta().failure_policy == gatel_plan::FailurePolicy::Open
            })
            .collect();
        assert_eq!(open, ["access-log"]);
    }
}
