//! The standard directive bundle.
//!
//! Each directive here is three small pieces: a struct with
//! `#[derive(FromNode)]` for its configuration, a `DirectiveConfig` impl that
//! says how it hashes and what handler it becomes, and a `Directive` impl
//! carrying its metadata. Nothing in a directive decides its own position in
//! the chain — the phase in its metadata does.

#![forbid(unsafe_code)]

pub mod access_log;
pub mod basic_auth;
pub mod encode;
pub mod file_server;
pub mod headers;
pub mod ip_filter;
pub mod observe;
pub mod rate_limit;
pub mod redirect;
pub mod request_id;
pub mod respond;
pub mod rewrite;
#[cfg(test)]
pub(crate) mod testing;

use gatel_plan::Bundle;

/// Every directive this crate provides.
pub fn bundle() -> Bundle {
    Bundle::new("std", "1")
        .with(respond::Respond)
        .with(redirect::Redirect)
        .with(headers::Headers)
        .with(access_log::AccessLog)
        .with(request_id::RequestId)
        .with(ip_filter::IpFilter)
        .with(rate_limit::RateLimit)
        .with(file_server::FileServerDirective)
        .with(encode::Encode)
        .with(rewrite::Rewrite)
        .with(basic_auth::BasicAuth)
        .with(observe::Metrics)
        .with(observe::Health)
}

#[cfg(test)]
mod tests {
    use gatel_plan::{Registry, Role};

    use super::*;

    #[test]
    fn the_bundle_builds_and_claims_distinct_names() {
        let registry = Registry::builder().bundle(bundle()).build().unwrap();
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(
            names,
            [
                "access-log",
                "basic-auth",
                "encode",
                "file-server",
                "headers",
                "health",
                "ip-filter",
                "metrics",
                "rate-limit",
                "redirect",
                "request-id",
                "respond",
                "rewrite"
            ]
        );
    }

    #[test]
    fn exactly_the_answering_directives_are_terminals() {
        let registry = Registry::builder().bundle(bundle()).build().unwrap();
        let terminals: Vec<&str> = registry
            .names()
            .filter(|name| registry.get(name).unwrap().meta().role == Role::Terminal)
            .collect();
        assert_eq!(
            terminals,
            ["file-server", "health", "metrics", "redirect", "respond"]
        );
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
