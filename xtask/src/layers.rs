use std::collections::{BTreeMap, BTreeSet};

/// What each crate is allowed to depend on directly.
///
/// The point of the low layers is what they *cannot* reach: `gatel-model` has
/// no async runtime and no I/O, and `gatel-plan` cannot open a socket or
/// terminate TLS, so neither can accidentally grow a side entrance into the
/// data plane.
pub(crate) const ALLOWED: &[(&str, &[&str])] = &[
    (
        "gatel-decode",
        &[
            "gatel-model",
            "gatel-macros",
            "gatel-plugin-api",
            "gatel-plugin-runtime",
            "salvo_core",
            "async-compression",
            "bytes",
            "futures-util",
            "http-body-util",
            "tokio",
            "tokio-util",
        ],
    ),
    ("gatel-password", &["bcrypt", "argon2", "zeroize"]),
    ("gatel-process-windows", &["windows"]),
    (
        "gatel-cgi",
        &[
            "gatel-cgi-http",
            "gatel-cgi-protocol",
            "gatel-plugin-runtime",
            "futures-util",
            "nix",
            "process-wrap",
            "gatel-process-windows",
            "tokio",
        ],
    ),
    (
        "gatel-scgi",
        &[
            "gatel-cgi-protocol",
            "gatel-cgi-http",
            "gatel-plugin-runtime",
            "tokio",
        ],
    ),
    (
        "gatel-fastcgi",
        &[
            "gatel-cgi-protocol",
            "gatel-cgi-http",
            "gatel-plugin-runtime",
            "tokio",
        ],
    ),
    (
        "gatel-cgi-http",
        &[
            "gatel-cgi-protocol",
            "tokio",
            "gatel-model",
            "gatel-macros",
            "gatel-plugin-runtime",
            "salvo_core",
            "http",
            "http-body",
            "http-body-util",
            "futures-util",
            "bytes",
            "tokio-util",
        ],
    ),
    ("gatel-cgi-protocol", &["http"]),
    // The host assembles packages; packages must never depend on the host.
    (
        "gatel",
        &[
            "hyper",
            "hyper-util",
            "bytes",
            "http",
            "http-body",
            "http-body-util",
            "subtle",
            // Optional Admin wire schema serialization belongs at the host edge.
            "serde_json",
            "async-compression",
            "gatel-decode",
            "gatel-cgi",
            "gatel-scgi",
            "gatel-fastcgi",
            "gatel-maintenance",
            "gatel-bundle-std",
            "gatel-http",
            "gatel-kdl",
            "gatel-model",
            "gatel-observe",
            "gatel-plan",
            "gatel-state",
            "gatel-tls",
            "clap",
            "clap_complete",
            "gatel-password",
            "zeroize",
            "clap_mangen",
            "notify",
            "tokio",
            "tracing",
            "tracing-subscriber",
        ],
    ),
    (
        "gatel-maintenance",
        &[
            "gatel-model",
            "gatel-macros",
            "gatel-plugin-runtime",
            "gatel-plugin-api",
            "salvo_core",
        ],
    ),
    ("gatel-plugin-api", &["gatel-model"]),
    (
        "gatel-plugin-runtime",
        &[
            "gatel-model",
            "gatel-plugin-api",
            "gatel-observe",
            "salvo_core",
            "tokio",
        ],
    ),
    ("gatel-model", &["blake3"]),
    ("gatel-macros", &["proc-macro2", "quote", "syn", "gatel-model"]),
    ("gatel-kdl", &["gatel-model", "kdl"]),
    // Below everything that can open a socket, so the data plane can write to it.
    ("gatel-observe", &["gatel-model", "time"]),
    (
        "gatel-plan",
        // salvo_core is the project's execution vocabulary; hyper, rustls and
        // tokio::net are transport and belong below.
        &[
            "gatel-plugin-runtime",
            "gatel-model",
            "gatel-macros",
            "gatel-kdl",
            "gatel-plugin-api",
            "gatel-observe",
            "salvo_core",
        ],
    ),
    (
        "gatel-state",
        &[
            "gatel-model",
            "gatel-plan",
            "gatel-observe",
            // The pool dials backends, and some of them speak TLS. The client
            // side of TLS lives with the server side, not in a second place.
            "gatel-tls",
            "gatel-kdl",
            "bytes",
            "http",
            "http-body-util",
            "hyper",
            "hyper-util",
            "salvo_core",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-bundle-std",
        &[
            "gatel-model",
            "gatel-macros",
            "gatel-plan",
            "gatel-observe",
            "async-compression",
            "gatel-password",
            // Pure bounded HTTP-date parsing for cache age and Expires.
            "httpdate",
            // Relative response URI resolution and same-origin checks; no I/O.
            "url",
            "bytes",
            "futures-util",
            "http-body-util",
            "hyper",
            "tokio-util",
            "http",
            "http-body",
            "pin-project-lite",
            "salvo_core",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-http",
        &[
            "gatel-model",
            "gatel-plan",
            "gatel-observe",
            "gatel-state",
            "gatel-tls",
            "gatel-kdl",
            "gatel-bundle-std",
            "bytes",
            "futures-util",
            "http",
            "http-body",
            "http-body-util",
            "hyper",
            "hyper-util",
            "pin-project-lite",
            "salvo_core",
            "socket2",
            "tokio",
            "tracing",
        ],
    ),
    (
        "gatel-tls",
        &[
            "gatel-model",
            "gatel-plan",
            // The ACME protocol itself. Same organisation, same licence, and
            // already what v1 uses: one implementation rather than two.
            "certon",
            // Validate certon's registration schema inside bounded atomic
            // account records; no configuration or host-layer dependency.
            "serde_json",
            "async-trait",
            "gatel-observe",
            "rcgen",
            "rustls",
            "time",
            "tokio",
            "tokio-rustls",
            "tracing",
            "webpki-roots",
        ],
    ),
];

/// The permission graph itself must remain a DAG, even if a forbidden cycle
/// has not yet been exercised by Cargo manifests.
pub(crate) fn validate(rules: &[(&str, &[&str])]) -> Vec<String> {
    let mut failures = Vec::new();
    let mut graph = BTreeMap::new();
    for &(name, dependencies) in rules {
        if graph.insert(name, dependencies).is_some() {
            failures.push(format!("duplicate architecture rule for {name}"));
        }
        let mut unique = BTreeSet::new();
        for &dependency in dependencies {
            if !unique.insert(dependency) {
                failures.push(format!("{name}: duplicate permitted dependency {dependency}"));
            }
        }
    }
    let mut remaining: BTreeSet<_> = graph.keys().copied().collect();
    loop {
        let ready: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|name| {
                graph[name]
                    .iter()
                    .all(|dependency| !remaining.contains(dependency))
            })
            .collect();
        if ready.is_empty() {
            break;
        }
        for name in ready {
            remaining.remove(name);
        }
    }
    if !remaining.is_empty() {
        failures.push(format!(
            "architecture permissions contain a cycle; blocked layers: {}",
            remaining.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_permissions_are_acyclic_and_unique() {
        assert!(validate(ALLOWED).is_empty());
    }
    #[test]
    fn duplicate_layers_and_edges_are_not_silently_overwritten() {
        let errors = validate(&[("a", &["external", "external"]), ("a", &[])]);
        assert_eq!(errors.len(), 2);
    }
    #[test]
    fn cycle_and_self_reference_are_rejected() {
        assert!(!validate(&[("a", &["b"]), ("b", &["a"])]).is_empty());
        assert!(!validate(&[("a", &["a"])]).is_empty());
    }
    #[test]
    fn external_dependencies_and_shared_lower_layers_are_allowed() {
        assert!(
            validate(&[
                ("top", &["left", "right"]),
                ("left", &["base"]),
                ("right", &["base"]),
                ("base", &["external"])
            ])
            .is_empty()
        );
    }
}
