use super::*;

#[test]
fn invalid_policy_bounds_are_rejected_instead_of_clamped() {
    for policy in [
        r#"retry budget-percent=101"#,
        r#"retry per-try="0s""#,
        r#"health "tcp" healthy-threshold=0"#,
        r#"health "tcp" unhealthy-threshold=0"#,
        r#"health "tcp" interval="0s" timeout="0s""#,
        r#"health "tcp" timeout="0s""#,
        r#"health "http" path="/" status=99"#,
        r#"health "http" path="/" status=600"#,
        r#"passive-health max-fails=0"#,
        r#"pool max-h2-streams-per-connection=0"#,
        r#"pool max-h2-streams-per-endpoint=0"#,
    ] {
        let errors = normalize(
            &snapshot(&format!(
                r#"global {{ http ":8080"; }}; upstream "pool" {{ endpoint "127.0.0.1:9000"; {policy}; }}"#
            )),
            &registry(),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| error.code == Code::InvalidValue),
            "{policy}: {errors}"
        );
    }
}

#[test]
fn h2_stream_limits_have_typed_defaults_and_preserve_positive_u32_values() {
    for policy in ["", "pool;"] {
        let result = spec(&format!(
            r#"global {{ http ":8080"; }}; upstream "pool" {{ endpoint "127.0.0.1:9000"; {policy} }}"#
        ));
        let pool = result.upstreams[0].pool;
        assert_eq!(pool.max_h2_streams_per_connection, 128);
        assert_eq!(pool.max_h2_streams_per_endpoint, 128);
    }
    for (connection, endpoint) in [(1, 2), (257, 1025), (u32::MAX, u32::MAX)] {
        let result = spec(&format!(
            r#"global {{ http ":8080"; }}; upstream "pool" {{ endpoint "127.0.0.1:9000";
            pool max-h2-streams-per-connection={connection} max-h2-streams-per-endpoint={endpoint}; }}"#
        ));
        let pool = result.upstreams[0].pool;
        assert_eq!(pool.max_h2_streams_per_connection, connection);
        assert_eq!(pool.max_h2_streams_per_endpoint, endpoint);
    }
}

#[test]
fn legitimate_policy_boundaries_are_preserved_exactly() {
    for percent in [0, 100] {
        let result = spec(&format!(
            r#"global {{ http ":8080"; }}; upstream "pool" {{
            endpoint "127.0.0.1:9000"; retry budget-percent={percent} per-try="1ms";
            health "http" path="/" status=599 interval="1ms" timeout="1ms" healthy-threshold=1 unhealthy-threshold=1 jitter="0s";
            passive-health max-fails=1;
        }}"#
        ));
        let upstream = &result.upstreams[0];
        assert_eq!(upstream.retry.as_ref().unwrap().budget_percent, percent);
        assert_eq!(upstream.health.as_ref().unwrap().healthy_threshold, 1);
        assert!(upstream.health.as_ref().unwrap().jitter.is_zero());
        assert_eq!(upstream.passive_health.as_ref().unwrap().max_fails, 1);
    }
}

#[test]
fn singleton_upstream_policies_are_never_first_declaration_wins() {
    for policy in [
        "balance \"round-robin\"",
        "timeouts connect=\"1s\"",
        "pool max-waiters=1",
        "retry attempts=1",
        "health \"tcp\"",
        "passive-health max-fails=1",
        "protocol \"http1\"",
        "resolve interval=\"30s\"",
        "tls server-name=\"example.com\"",
    ] {
        for named in [true, false] {
            let body = format!("endpoint \"example.com:443\"; {policy}; {policy};");
            let text = if named {
                format!(
                    r#"global {{ http ":8080"; }}; upstream "pool" {{ {body} }}; site "*" {{ route "/" {{ proxy upstream="pool"; }}; }}"#
                )
            } else {
                format!(r#"global {{ http ":8080"; }}; site "*" {{ route "/" {{ proxy {{ {body} }}; }}; }}"#)
            };
            let errors = normalize(&snapshot(&text), &registry()).unwrap_err();
            assert!(
                errors.iter().any(|error| error.code == Code::Duplicate),
                "{policy}: {errors}"
            );
        }
    }
}

#[test]
fn manual_upstream_decoders_reject_ignored_input() {
    for (policy, code) in [
        (r#"balance "random" "ignored";"#, Code::ArgumentCount),
        (r#"balance "random" { ignored; };"#, Code::UnknownNode),
        (r#"protocol "http1" "http2";"#, Code::ArgumentCount),
        (r#"protocol "http1" ignored=#true;"#, Code::UnknownProperty),
        (r#"protocol "http1" { ignored; };"#, Code::UnknownNode),
        (r#"tls "ignored";"#, Code::ArgumentCount),
        (r#"tls server-name=123;"#, Code::InvalidValue),
        (
            r#"tls server-name="example.com" { ignored; };"#,
            Code::UnknownNode,
        ),
    ] {
        let errors = normalize(
            &snapshot(&format!(
                r#"global {{ http ":8080"; }}; upstream "pool" {{ endpoint "example.com:443"; {policy} }}"#
            )),
            &registry(),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| error.code == code),
            "{policy}: {errors}"
        );
    }
    let errors =
        errors(r#"global { http ":8080"; }; upstream "pool" "ignored" { endpoint "127.0.0.1:9000"; }"#);
    assert!(errors.iter().any(|error| error.code == Code::ArgumentCount));
}

#[test]
fn model_producers_cannot_bypass_duplicate_tls_property_checks() {
    let mut input = snapshot(
        r#"global { http ":8080"; }; upstream "pool" { endpoint "example.com:443"; tls server-name="example.com"; }"#,
    );
    let mut source = input.sources().next().unwrap().clone();
    let tls = &mut source.nodes[1].children[1];
    tls.props.push(tls.props[0].clone());
    input.insert(source);
    let errors = normalize(&input, &registry()).unwrap_err();
    assert!(errors.iter().any(|error| error.code == Code::Duplicate));
}

#[test]
fn endpoint_lists_and_distinct_policies_remain_valid() {
    let result = spec(
        r#"global { http ":8080"; }; upstream "pool" {
        endpoint "127.0.0.1:9000"; endpoint "127.0.0.1:9001";
        balance "random"; timeouts connect="1s"; pool max-waiters=3;
        protocol "http1";
    }"#,
    );
    assert_eq!(result.upstreams[0].endpoints.len(), 2);
}
