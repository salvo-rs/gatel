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
