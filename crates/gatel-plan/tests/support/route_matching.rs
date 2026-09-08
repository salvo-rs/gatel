use super::*;

#[test]
fn two_routes_matching_the_same_requests_are_rejected() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    route "/api/*" { respond "a" }
    route "/api/*" { respond "b" }
}
"#,
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == Code::Duplicate && d.message.contains("exactly the same requests"))
    );
}

#[test]
fn a_match_condition_makes_two_routes_distinguishable() {
    let spec = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/api/*" {
        match header="x-canary=1"
        respond "canary"
    }
    route "/api/*" { respond "stable" }
}
"#,
    );
    assert_eq!(spec.sites[0].routes.len(), 2);
    // The one with an extra condition is tried first.
    assert_eq!(spec.sites[0].routes[0].matchers.len(), 1);
}
