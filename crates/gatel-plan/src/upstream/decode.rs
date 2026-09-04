//! Decoding an upstream.
//!
//! One decoder serves three spellings: a named `upstream` block, an inline
//! `proxy { .. }` body, and the `proxy "address"` short form, which is lowered
//! into a one-endpoint body before it gets here. Keeping them on one path is
//! what stops the short form from quietly having different defaults.

use gatel_model::{Code, Diagnostics, FromNode as _, Node, ResourceId, ResourceKind, SourceId};

use super::{
    Balance, Endpoint, EndpointNode, Health, HealthKind, HealthNode, PassiveHealth, PassiveHealthNode, Pool,
    PoolNode, Retry, RetryMode, RetryNode, Timeouts, TimeoutsNode, Upstream, UpstreamProtocol, UpstreamTls,
};

pub fn decode_upstream_body(
    node: &Node,
    id: ResourceId,
    owner: SourceId,
    diagnostics: &mut Diagnostics,
) -> Option<Upstream> {
    let errors_before = diagnostics.error_count();
    let mut endpoints: Vec<Endpoint> = Vec::new();

    let declared = node.children_named("endpoint").count();
    for child in node.children_named("endpoint") {
        let Some(decoded) = EndpointNode::from_node(child, diagnostics) else {
            continue;
        };
        let address = match gatel_model::parse_socket_addr(&decoded.address) {
            Ok(address) => address,
            Err(message) => {
                let mut diagnostic =
                    gatel_model::Diagnostic::error(Code::InvalidValue, child.args[0].origin.clone(), message);
                // The question this always raises next.
                if decoded.address.split(':').next().is_some_and(is_name) {
                    diagnostic = diagnostic.with_help(
                        "host names are not resolved yet: give the address, and if the backend speaks TLS put its name in `tls server-name=`",
                    );
                }
                diagnostics.push(diagnostic);
                continue;
            }
        };
        if decoded.weight == 0 {
            diagnostics.error(
                Code::InvalidValue,
                child.origin.clone(),
                "`weight` must be at least 1; use a separate upstream to take a backend out",
            );
            continue;
        }
        let id = decoded.id.unwrap_or_else(|| address.to_string());
        if let Some(previous) = endpoints.iter().find(|existing| existing.id == id) {
            diagnostics.push(
                gatel_model::Diagnostic::error(
                    Code::Duplicate,
                    child.origin.clone(),
                    format!("endpoint `{id}` is declared twice"),
                )
                .with_related("first declared here", previous.origin.clone()),
            );
            continue;
        }
        endpoints.push(Endpoint {
            id,
            address,
            weight: decoded.weight,
            priority: decoded.priority,
            origin: child.origin.clone(),
        });
    }

    // Only when none were written. Endpoints that were written and refused
    // have already said why, one message each.
    if endpoints.is_empty() && declared == 0 {
        diagnostics.push(
            gatel_model::Diagnostic::error(
                Code::MissingField,
                node.origin.clone(),
                "an upstream needs at least one `endpoint`",
            )
            .with_help("write `endpoint \"127.0.0.1:8080\"`"),
        );
    }

    let balance = match node.child("balance") {
        Some(child) => {
            let Some(name) = child.args.first().and_then(|value| value.as_str()) else {
                diagnostics.error(
                    Code::ArgumentCount,
                    child.origin.clone(),
                    "`balance` takes the strategy name as its argument",
                );
                return None;
            };
            Balance::parse(name, child, diagnostics)?
        }
        None => Balance::default(),
    };

    let timeouts = match node.child("timeouts") {
        Some(child) => {
            let decoded = TimeoutsNode::from_node(child, diagnostics)?;
            let base = Timeouts::default();
            let timeouts = Timeouts {
                queue: decoded.queue.unwrap_or(base.queue),
                connect: decoded.connect.unwrap_or(base.connect),
                response_head: decoded.response_head.unwrap_or(base.response_head),
                body_idle: decoded.body_idle.unwrap_or(base.body_idle),
                overall: decoded.overall.unwrap_or(base.overall),
                upgrade_idle: decoded.upgrade_idle.unwrap_or(base.upgrade_idle),
                upgrade_duration: decoded.upgrade_duration.unwrap_or(base.upgrade_duration),
            };
            timeouts.validate(&child.origin, diagnostics);
            timeouts
        }
        None => Timeouts::default(),
    };

    let pool = match node.child("pool") {
        Some(child) => {
            let decoded = PoolNode::from_node(child, diagnostics)?;
            let base = Pool::default();
            let pool = Pool {
                max_per_endpoint: decoded.max_per_endpoint.unwrap_or(base.max_per_endpoint),
                max_idle_per_endpoint: decoded
                    .max_idle_per_endpoint
                    .unwrap_or(base.max_idle_per_endpoint),
                idle_timeout: decoded.idle_timeout.unwrap_or(base.idle_timeout),
                max_lifetime: decoded.max_lifetime.unwrap_or(base.max_lifetime),
                max_waiters: decoded.max_waiters.unwrap_or(base.max_waiters),
            };
            if pool.max_per_endpoint == 0 {
                diagnostics.error(
                    Code::InvalidValue,
                    child.origin.clone(),
                    "`max-per-endpoint` must be at least 1",
                );
            }
            if pool.max_idle_per_endpoint > pool.max_per_endpoint {
                diagnostics.error(
                    Code::InvalidValue,
                    child.origin.clone(),
                    "`max-idle-per-endpoint` cannot exceed `max-per-endpoint`",
                );
            }
            pool
        }
        None => Pool::default(),
    };

    let retry = match node.child("retry") {
        Some(child) => {
            let decoded = RetryNode::from_node(child, diagnostics)?;
            let mode = match decoded.mode.as_str() {
                "safe" => RetryMode::Safe,
                "replay" => RetryMode::Replay,
                other => {
                    diagnostics.push(
                        gatel_model::Diagnostic::error(
                            Code::InvalidValue,
                            child.origin.clone(),
                            format!("unknown retry mode `{other}`"),
                        )
                        .with_help(
                            "`safe` retries only pre-send failures, `replay` also replays buffered bodies",
                        ),
                    );
                    return None;
                }
            };
            if mode == RetryMode::Replay && decoded.buffer.is_none() {
                diagnostics.push(
                    gatel_model::Diagnostic::error(
                        Code::MissingField,
                        child.origin.clone(),
                        "`retry mode=\"replay\"` requires an explicit `buffer`",
                    )
                    .with_help("replaying a request can duplicate it, so the cost must be stated"),
                );
                return None;
            }
            if decoded.attempts < 1 || decoded.attempts > 5 {
                diagnostics.error(
                    Code::InvalidValue,
                    child.origin.clone(),
                    "`attempts` must be between 1 and 5",
                );
            }
            Some(Retry {
                mode,
                attempts: decoded.attempts,
                per_try: decoded.per_try,
                buffer: decoded.buffer.unwrap_or_default(),
                budget_percent: decoded.budget_percent.min(100),
            })
        }
        None => None,
    };

    let health = match node.child("health") {
        Some(child) => {
            let decoded = HealthNode::from_node(child, diagnostics)?;
            let kind = match decoded.kind.as_str() {
                "tcp" | "tcp-connect" => {
                    for absent in ["path", "status", "host"] {
                        if let Some(property) = child.prop(absent) {
                            diagnostics.error(
                                Code::UnknownProperty,
                                property.origin.clone(),
                                format!("`health \"tcp\"` has no property `{absent}`"),
                            );
                        }
                    }
                    HealthKind::TcpConnect
                }
                "http" => {
                    let Some(path) = decoded.path else {
                        diagnostics.error(
                            Code::MissingField,
                            child.origin.clone(),
                            "`health \"http\"` requires a `path`",
                        );
                        return None;
                    };
                    if !path.starts_with('/') {
                        diagnostics.error(
                            Code::InvalidValue,
                            child.origin.clone(),
                            "health check `path` must start with `/`",
                        );
                    }
                    HealthKind::Http {
                        path,
                        status: decoded.status.unwrap_or(200),
                        host: decoded.host,
                    }
                }
                other => {
                    diagnostics.push(
                        gatel_model::Diagnostic::error(
                            Code::InvalidValue,
                            child.origin.clone(),
                            format!("unknown health check `{other}`"),
                        )
                        .with_help("known checks: tcp, http"),
                    );
                    return None;
                }
            };
            if decoded.timeout > decoded.interval {
                diagnostics.error(
                    Code::InvalidValue,
                    child.origin.clone(),
                    "health check `timeout` must not exceed `interval`",
                );
            }
            Some(Health {
                kind,
                interval: decoded.interval,
                timeout: decoded.timeout,
                healthy_threshold: decoded.healthy_threshold.max(1),
                unhealthy_threshold: decoded.unhealthy_threshold.max(1),
                jitter: decoded.jitter.unwrap_or(decoded.interval / 10),
            })
        }
        None => None,
    };

    let passive_health = match node.child("passive-health") {
        Some(child) => {
            let decoded = PassiveHealthNode::from_node(child, diagnostics)?;
            let unhealthy_status = match decoded.unhealthy_status {
                Some(raw) => {
                    let mut parsed = Vec::new();
                    for part in raw.split(',') {
                        match part.trim().parse::<u16>() {
                            Ok(status) if (100..=599).contains(&status) => parsed.push(status),
                            _ => diagnostics.error(
                                Code::InvalidValue,
                                child.origin.clone(),
                                format!("`{part}` is not an HTTP status code"),
                            ),
                        }
                    }
                    parsed
                }
                None => vec![502, 503, 504],
            };
            Some(PassiveHealth {
                max_fails: decoded.max_fails.max(1),
                window: decoded.window,
                cooldown: decoded.cooldown,
                unhealthy_status,
            })
        }
        None => None,
    };

    let protocol = match node.child("protocol") {
        Some(child) => match child.args.first().and_then(|value| value.as_str()) {
            Some("http1") => UpstreamProtocol::Http1,
            Some("http2") => UpstreamProtocol::Http2,
            other => {
                diagnostics.push(
                    gatel_model::Diagnostic::error(
                        Code::InvalidValue,
                        child.origin.clone(),
                        format!("unknown upstream protocol `{}`", other.unwrap_or("")),
                    )
                    .with_help("known protocols: http1, http2"),
                );
                return None;
            }
        },
        None => UpstreamProtocol::default(),
    };

    let tls = match node.child("tls") {
        Some(child) => {
            let Some(server_name) = child
                .prop("server-name")
                .and_then(|property| property.value.as_str())
            else {
                diagnostics.push(
                    gatel_model::Diagnostic::error(
                        Code::MissingField,
                        child.origin.clone(),
                        "upstream `tls` requires `server-name`",
                    )
                    .with_help("it is both the SNI value and the name the certificate is checked against"),
                );
                return None;
            };
            for property in &child.props {
                if !["server-name", "ca", "client-cert", "client-key"].contains(&property.name.as_str()) {
                    let mut diagnostic = gatel_model::Diagnostic::error(
                        Code::UnknownProperty,
                        property.origin.clone(),
                        format!("upstream `tls` has no property `{}`", property.name),
                    );
                    if property.name.contains("insecure") || property.name.contains("skip") {
                        diagnostic = diagnostic.with_help(
                            "certificate verification cannot be disabled; use `tls internal` for development",
                        );
                    }
                    diagnostics.push(diagnostic);
                }
            }
            let client_cert = secret_property(child, "client-cert", diagnostics);
            let client_key = secret_property(child, "client-key", diagnostics);
            if client_cert.is_some() != client_key.is_some() {
                diagnostics.error(
                    Code::MissingField,
                    child.origin.clone(),
                    "`client-cert` and `client-key` must be given together",
                );
            }
            Some(UpstreamTls {
                server_name: server_name.to_string(),
                ca: secret_property(child, "ca", diagnostics),
                client_cert,
                client_key,
            })
        }
        None => None,
    };

    for child in &node.children {
        const KNOWN: [&str; 9] = [
            "endpoint",
            "balance",
            "timeouts",
            "pool",
            "retry",
            "health",
            "passive-health",
            "protocol",
            "tls",
        ];
        if !KNOWN.contains(&child.name.as_str()) {
            let mut diagnostic = gatel_model::Diagnostic::error(
                Code::UnknownNode,
                child.origin.clone(),
                format!("`{}` is not part of an upstream", child.name),
            );
            if let Some(suggestion) = gatel_model::did_you_mean(&child.name, KNOWN) {
                diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
            }
            diagnostics.push(diagnostic);
        }
    }

    if diagnostics.error_count() > errors_before {
        return None;
    }

    Some(Upstream {
        id,
        owner,
        endpoints,
        balance,
        timeouts,
        pool,
        retry,
        health,
        passive_health,
        protocol,
        tls,
        origin: node.origin.clone(),
    })
}

/// Read an optional secret reference property.
fn secret_property(node: &Node, name: &str, diagnostics: &mut Diagnostics) -> Option<gatel_model::SecretRef> {
    let property = node.prop(name)?;
    let text = property.value.as_str()?;
    match gatel_model::SecretRef::parse(text) {
        Ok(reference) => Some(reference),
        Err(message) => {
            diagnostics.error(Code::InvalidSecret, property.origin.clone(), message);
            None
        }
    }
}

/// The identity of an upstream declared inline inside a `proxy` node.
pub fn anonymous_upstream_id(route: &ResourceId, addresses: &[String]) -> ResourceId {
    let mut parts: Vec<&str> = addresses.iter().map(String::as_str).collect();
    parts.sort();
    let mut key = vec![route.name()];
    key.extend(parts);
    ResourceId::derived(ResourceKind::Upstream, &key)
}

/// Whether this looks like a host name rather than a typo of an address.
///
/// Only used to decide whether to explain the limitation, so being generous
/// costs nothing: the message is help on an error that is already correct.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.parse::<std::net::IpAddr>().is_err()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '.')
}
