use super::*;

fn source(owner: SourceId, text: &str) -> SourceSnapshot {
    let fixture = snapshot(text);
    let mut source = fixture.sources().next().unwrap().clone();
    source.source = owner;
    source
}

#[test]
fn explicit_site_and_route_ids_are_unique_across_the_whole_configuration() {
    for text in [
        r#"global { http ":8080"; }; site "a.example" id="same" { route "/" { respond "a"; }; }; site "b.example" id="same" { route "/" { respond "b"; }; }"#,
        r#"global { http ":8080"; }; site "a.example" { route "/a" id="same" { respond "a"; }; }; site "b.example" { route "/b" id="same" { respond "b"; }; }"#,
        r#"global { http ":8080"; }; site "*" { route "/a" id="same" { respond "a"; }; route "/b" id="same" { respond "b"; }; }"#,
    ] {
        let errors = normalize(&snapshot(text), &registry()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.code == Code::Duplicate && error.message.contains("resource ID")),
            "{errors}"
        );
    }
}

#[test]
fn matcher_specific_routes_have_distinct_stable_implicit_identities() {
    let build = |reverse| {
        let matchers = if reverse {
            "match query=\"version=1\"; match header=\"x-canary=1\";"
        } else {
            "match header=\"x-canary=1\"; match query=\"version=1\";"
        };
        spec(&format!(
            r#"global {{ http ":8080"; }}; site "*" {{ route "/" {{ {matchers} respond "canary"; }}; route "/" {{ respond "stable"; }}; }}"#
        ))
    };
    let first = build(false);
    let second = build(true);
    let conditional = |spec: &Spec| {
        spec.sites[0]
            .routes
            .iter()
            .find(|route| !route.matchers.is_empty())
            .unwrap()
            .id
            .clone()
    };
    assert_eq!(conditional(&first), conditional(&second));
    assert_ne!(first.sites[0].routes[0].id, first.sites[0].routes[1].id);
}

#[test]
fn resource_kinds_and_namespaces_do_not_share_uniqueness_domains() {
    let mut input = snapshot(
        r#"global { http ":8080"; }; site "file.example" id="same" { route "/" id="same" { respond "ok"; }; }"#,
    );
    input.insert(source(
        SourceId::admin(),
        r#"site "runtime.example" id="runtime/same" { route "/" id="runtime/same" { respond "ok"; }; }"#,
    ));
    assert!(normalize(&input, &registry()).is_ok());
}

#[test]
fn implicit_sites_routes_and_upstreams_obey_source_ownership() {
    let mut site_ids = std::collections::BTreeSet::new();
    let mut route_ids = std::collections::BTreeSet::new();
    let mut upstream_ids = std::collections::BTreeSet::new();
    for owner in [
        SourceId::admin(),
        SourceId::kubernetes("a"),
        SourceId::kubernetes("b"),
    ] {
        let mut input = snapshot(r#"global { http ":8080"; }"#);
        input.insert(source(
            owner.clone(),
            r#"site "same.example" { route "/" { proxy "127.0.0.1:9000"; }; }"#,
        ));
        let spec = normalize(&input, &registry()).unwrap();
        let site = &spec.sites[0];
        let route = &site.routes[0];
        let upstream = &spec.upstreams[0];
        for id in [&site.id, &route.id, &upstream.id] {
            assert!(owner.owns(id.name()), "{owner} does not own {id}");
            assert!(id.is_derived());
        }
        assert!(site_ids.insert(site.id.clone()));
        assert!(route_ids.insert(route.id.clone()));
        assert!(upstream_ids.insert(upstream.id.clone()));
        assert!(matches!(&route.terminal, Terminal::Proxy { upstream: id } if *id == upstream.id));
    }
}

#[test]
fn namespaced_inline_upstreams_still_share_within_a_site_and_ignore_address_order() {
    let build = |reverse| {
        let addresses = if reverse {
            "{ endpoint \"127.0.0.1:9001\"; endpoint \"127.0.0.1:9000\"; }"
        } else {
            "{ endpoint \"127.0.0.1:9000\"; endpoint \"127.0.0.1:9001\"; }"
        };
        let mut input = snapshot(r#"global { http ":8080"; }"#);
        input.insert(source(SourceId::admin(), &format!(r#"site "runtime.example" {{ route "/a" {{ proxy {addresses}; }}; route "/b" {{ proxy {addresses}; }}; }}"#)));
        normalize(&input, &registry()).unwrap()
    };
    let first = build(false);
    let reordered = build(true);
    assert_eq!(first.upstreams.len(), 1);
    assert_eq!(reordered.upstreams.len(), 1);
    assert_eq!(first.upstreams[0].id, reordered.upstreams[0].id);
    assert!(SourceId::admin().owns(first.upstreams[0].id.name()));
    for route in &first.sites[0].routes {
        assert!(
            matches!(&route.terminal, Terminal::Proxy { upstream } if *upstream == first.upstreams[0].id)
        );
    }
}

#[test]
fn namespaced_sources_cannot_supply_global_host_settings() {
    for owner in [SourceId::admin(), SourceId::kubernetes("team")] {
        let input = Snapshot::new().with(source(owner, r#"global { http ":8080"; blocking jobs=100; }"#));
        let errors = normalize(&input, &registry()).unwrap_err();
        assert!(errors.iter().any(|error| error.code == Code::OwnershipConflict));
        assert!(errors.iter().all(|error| !error.message.contains("100")));
    }
}

#[test]
fn proxy_never_silently_discards_extra_arguments_or_properties() {
    for (proxy, code) in [
        (r#"proxy "127.0.0.1:9000" "127.0.0.1:9001";"#, Code::ArgumentCount),
        (
            r#"proxy "127.0.0.1:9000" misspelled="value";"#,
            Code::UnknownProperty,
        ),
        (
            r#"proxy upstream="pool" misspelled="value";"#,
            Code::UnknownProperty,
        ),
        (
            r#"proxy misspelled="value" { endpoint "127.0.0.1:9000"; };"#,
            Code::UnknownProperty,
        ),
    ] {
        let input = snapshot(&format!(
            r#"global {{ http ":8080"; }}; site "*" {{ route "/" {{ {proxy} }}; }}"#
        ));
        let errors = normalize(&input, &registry()).unwrap_err();
        assert!(errors.iter().any(|error| error.code == code), "{errors}");
    }
    // Duplicate properties may already be rejected by a concrete frontend;
    // model producers must receive the same protection.
    let mut nodes = gatel_kdl::lower(
        r#"global { http ":8080"; }; site "*" { route "/" { proxy upstream="pool"; }; }"#,
        "test",
    )
    .unwrap();
    let proxy = &mut nodes[1].children[0].children[0];
    proxy.props.push(proxy.props[0].clone());
    let input = Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        Hasher::new().finish(),
        nodes,
    ));
    let errors = normalize(&input, &registry()).unwrap_err();
    assert!(errors.iter().any(|error| error.code == Code::Duplicate));
}

#[test]
fn file_global_and_namespaced_resource_contributions_coexist() {
    let mut input = snapshot(r#"global { http ":8080"; }"#);
    input.insert(source(
        SourceId::admin(),
        r#"site "runtime.example" id="runtime/site" { route "/" id="runtime/route" { respond "ok"; }; }"#,
    ));
    input.insert(source(
        SourceId::kubernetes("team"),
        r#"site "team.example" id="k8s/team/site" { route "/" id="k8s/team/route" { respond "team"; }; }"#,
    ));
    let compiled = normalize(&input, &registry()).unwrap();
    assert_eq!(compiled.sites.len(), 2);
    assert_eq!(compiled.listeners[0].owner, SourceId::file("main"));
}

#[test]
fn adding_runtime_global_never_shadows_file_global() {
    let mut input = snapshot(r#"global { http ":8080"; }"#);
    input.insert(source(SourceId::admin(), r#"global { http ":9090"; }"#));
    let errors = normalize(&input, &registry()).unwrap_err();
    assert!(errors.iter().any(|error| error.code == Code::OwnershipConflict));
}
