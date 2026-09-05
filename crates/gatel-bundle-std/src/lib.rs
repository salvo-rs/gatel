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
pub mod body_limit;
pub mod cache;
pub mod cors;
pub mod encode;
pub mod error_pages;
pub mod file_server;
pub mod force_https;
pub mod forward_auth;
pub mod headers;
pub mod ip_filter;
pub mod observe;
pub mod ranges;
pub mod rate_limit;
pub mod redirect;
pub mod request_id;
pub mod respond;
pub mod rewrite;
#[cfg(test)]
pub(crate) mod testing;
pub mod timeout;
pub mod trailing_slash;

use gatel_plan::Bundle;

/// Mark this response as one the gateway produced rather than one a backend
/// answered with.
///
/// `error-pages` reads it, and so does anything else that has to tell "we
/// refused this" from "the thing behind us said no". Every refusal in this
/// crate sets it; forgetting to costs a custom error page, not correctness.
pub fn generated(depot: &mut salvo_core::Depot) {
    depot.insert("gatel.generated", true);
}

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
        .with(cache::Cache)
        .with(file_server::FileServerDirective)
        .with(encode::Encode)
        .with(rewrite::Rewrite)
        .with(basic_auth::BasicAuth)
        .with(cors::Cors)
        .with(error_pages::ErrorPages)
        .with(force_https::ForceHttps)
        .with(forward_auth::ForwardAuth)
        .with(trailing_slash::TrailingSlash)
        .with(timeout::Timeout)
        .with(body_limit::BodyLimit)
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
                "body-limit",
                "cache",
                "cors",
                "encode",
                "error-pages",
                "file-server",
                "force-https",
                "forward-auth",
                "headers",
                "health",
                "ip-filter",
                "metrics",
                "rate-limit",
                "redirect",
                "request-id",
                "respond",
                "rewrite",
                "timeout",
                "trailing-slash"
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
