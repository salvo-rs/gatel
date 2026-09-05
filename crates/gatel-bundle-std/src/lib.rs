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

/// The host the client asked for, port and all, or `None` if it did not say.
///
/// One function because there is one answer, and three directives were each
/// reaching for it slightly differently — which is how a cache key and a
/// redirect end up disagreeing about what site a request was for.
///
/// The URI comes first on purpose. HTTP/2 carries the authority there, and a
/// client may send a `host` header as well; when both are present the standard
/// says the authority is the one that counts.
pub fn authority(request: &salvo_core::http::Request) -> Option<&str> {
    if let Some(authority) = request.uri().authority() {
        return Some(authority.as_str());
    }
    request
        .headers()
        .get(salvo_core::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
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

    #[test]
    fn the_authority_in_the_uri_wins_over_a_host_header() {
        // HTTP/2 puts it in the URI and may carry a `host` header as well.
        // Reading the header first would let the two disagree, and a cache key
        // and a redirect would then be about different sites.
        let mut request = salvo_core::http::Request::default();
        *request.uri_mut() = "https://real.example/path".parse().unwrap();
        request.headers_mut().insert(
            salvo_core::http::header::HOST,
            salvo_core::http::HeaderValue::from_static("other.example"),
        );
        assert_eq!(authority(&request), Some("real.example"));
    }

    #[test]
    fn without_a_uri_authority_the_host_header_answers() {
        let mut request = salvo_core::http::Request::default();
        *request.uri_mut() = "/path".parse().unwrap();
        request.headers_mut().insert(
            salvo_core::http::header::HOST,
            salvo_core::http::HeaderValue::from_static("  example.com:8443  "),
        );
        assert_eq!(authority(&request), Some("example.com:8443"));
    }

    #[test]
    fn a_request_that_names_no_host_says_so() {
        // Rather than answering with an empty string, which every caller would
        // then have to recognise as "none" on its own.
        let request = salvo_core::http::Request::default();
        assert_eq!(authority(&request), None);
    }
}
