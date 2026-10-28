use super::*;

fn policies(reverse: bool) -> Spec {
    let fast = r#"route "/fast" { proxy { endpoint "127.0.0.1:9000"; timeouts response-head="1s"; pool max-per-endpoint=2 max-idle-per-endpoint=1; }; }"#;
    let slow = r#"route "/slow" { proxy { endpoint "127.0.0.1:9000"; timeouts response-head="5s"; pool max-per-endpoint=8; }; }"#;
    let routes = if reverse {
        format!("{slow}; {fast}")
    } else {
        format!("{fast}; {slow}")
    };
    spec(&format!(
        r#"global {{ http ":8080"; }}; site "*" {{ {routes}; }}"#
    ))
}

#[test]
fn equal_addresses_with_different_policies_never_use_first_declaration_wins() {
    for reverse in [false, true] {
        let spec = policies(reverse);
        assert_eq!(spec.upstreams.len(), 2);
        for route in &spec.sites[0].routes {
            let Terminal::Proxy { upstream: id } = &route.terminal else {
                panic!("proxy")
            };
            let upstream = spec.upstreams.iter().find(|upstream| upstream.id == *id).unwrap();
            let fast = route.path.describe() == "/fast";
            assert_eq!(
                upstream.timeouts.response_head.as_secs(),
                if fast { 1 } else { 5 }
            );
            assert_eq!(upstream.pool.max_per_endpoint, if fast { 2 } else { 8 });
        }
    }
    let identities = |spec: Spec| {
        spec.upstreams
            .into_iter()
            .map(|upstream| upstream.id)
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(identities(policies(false)), identities(policies(true)));
}

#[test]
fn shorthand_and_equivalent_decoded_defaults_share_one_upstream() {
    let spec = spec(
        r#"global { http ":8080"; }; site "*" {
        route "/a" { proxy "127.0.0.1:9000"; };
        route "/b" { proxy { endpoint "127.0.0.1:9000"; balance "round-robin"; }; };
    }"#,
    );
    assert_eq!(spec.upstreams.len(), 1);
}

#[test]
fn endpoint_weight_and_balance_participate_in_inline_identity() {
    let spec = spec(
        r#"global { http ":8080"; }; site "*" {
        route "/a" { proxy { endpoint "127.0.0.1:9000" weight=1; }; };
        route "/b" { proxy { endpoint "127.0.0.1:9000" weight=2; }; };
        route "/c" { proxy { endpoint "127.0.0.1:9000"; balance "random"; }; };
    }"#,
    );
    assert_eq!(spec.upstreams.len(), 3);
}

#[test]
fn each_h2_stream_limit_changes_inline_identity_and_explicit_defaults_share_it() {
    let result = spec(
        r#"global { http ":8080"; }; site "*" {
        route "/default" { proxy "127.0.0.1:9000"; };
        route "/explicit" { proxy { endpoint "127.0.0.1:9000"; pool max-h2-streams-per-connection=128 max-h2-streams-per-endpoint=128; }; };
        route "/connection" { proxy { endpoint "127.0.0.1:9000"; pool max-h2-streams-per-connection=7; }; };
        route "/endpoint" { proxy { endpoint "127.0.0.1:9000"; pool max-h2-streams-per-endpoint=7; }; };
        route "/both" { proxy { endpoint "127.0.0.1:9000"; pool max-h2-streams-per-connection=7 max-h2-streams-per-endpoint=7; }; };
    }"#,
    );
    assert_eq!(
        result.upstreams.len(),
        4,
        "both stream fields participate independently in the policy hash"
    );
    let identity = |path: &str| {
        let route = result.sites[0]
            .routes
            .iter()
            .find(|route| route.path.describe() == path)
            .unwrap();
        let Terminal::Proxy { upstream } = &route.terminal else {
            panic!("proxy")
        };
        upstream
    };
    assert_eq!(identity("/default"), identity("/explicit"));
    let identities = ["/default", "/connection", "/endpoint", "/both"].map(identity);
    assert_eq!(
        identities
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
}
