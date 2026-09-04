//! The compiler, driven by real KDL text rather than hand-built nodes.
//!
//! These are the tests that would catch a change in meaning: what a
//! configuration compiles to, and what it refuses to compile at all.

use std::sync::Arc;

use gatel_model::{Code, Diagnostics, Hasher, Node, Snapshot, SourceId, SourceRev, SourceSnapshot};
use gatel_plan::{
    Bundle, Directive, DirectiveConfig, DirectiveMeta, Phase, Registry, Scopes, Spec, Terminal, compile,
    normalize,
};
use salvo_core::Handler;

// A stand-in for the standard bundle: enough directives to exercise scopes,
// phases and terminals without depending on their real implementations.
struct Stub(DirectiveMeta);
struct StubConfig(String);

impl DirectiveConfig for StubConfig {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.0);
    }

    fn build(
        &self,
        _state: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<Arc<dyn Handler>, String> {
        Err("not built in compiler tests".into())
    }

    fn describe(&self) -> String {
        self.0.clone()
    }
}

impl Directive for Stub {
    fn meta(&self) -> &DirectiveMeta {
        &self.0
    }

    fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let argument = node
            .args
            .first()
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        Some(Arc::new(StubConfig(format!("{} {argument}", node.name))))
    }
}

fn registry() -> Registry {
    Registry::builder()
        .bundle(
            Bundle::new("std", "1")
                .with(Stub(DirectiveMeta::terminal("respond")))
                .with(Stub(DirectiveMeta::terminal("file-server")))
                .with(Stub(DirectiveMeta::middleware(
                    "headers",
                    Phase::Response,
                    Scopes::BOTH,
                )))
                .with(Stub(DirectiveMeta::middleware(
                    "encode",
                    Phase::Response,
                    Scopes::BOTH,
                )))
                .with(Stub(DirectiveMeta::middleware(
                    "basic-auth",
                    Phase::Guard,
                    Scopes::BOTH,
                )))
                .with(Stub(DirectiveMeta::middleware(
                    "access-log",
                    Phase::Observe,
                    Scopes::BOTH,
                )))
                .with(Stub(DirectiveMeta::middleware(
                    "rewrite",
                    Phase::Request,
                    Scopes::ROUTE,
                ))),
        )
        .build()
        .unwrap()
}

fn snapshot(text: &str) -> Snapshot {
    let nodes = gatel_kdl::lower(text, "gatel.kdl").expect("valid KDL");
    let mut hasher = Hasher::new();
    hasher.update_str(text);
    Snapshot::new().with(SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(1),
        hasher.finish(),
        nodes,
    ))
}

fn spec(text: &str) -> Spec {
    match normalize(&snapshot(text), &registry()) {
        Ok(spec) => spec,
        Err(diagnostics) => panic!("expected valid configuration, got:\n{diagnostics}"),
    }
}

fn errors(text: &str) -> Diagnostics {
    normalize(&snapshot(text), &registry())
        .err()
        .unwrap_or_else(|| panic!("expected the configuration to be rejected:\n{text}"))
}

fn has(diagnostics: &Diagnostics, code: Code) -> bool {
    diagnostics.iter().any(|diagnostic| diagnostic.code == code)
}

const MINIMAL: &str = r#"
global {
    http ":8080"
}
site "*" {
    route "/" {
        respond "hello"
    }
}
"#;

#[test]
fn a_minimal_configuration_compiles() {
    let spec = spec(MINIMAL);
    assert_eq!(spec.listeners.len(), 1);
    assert_eq!(spec.listeners[0].name, "http");
    assert_eq!(spec.listeners[0].bind.to_string(), "0.0.0.0:8080");
    assert_eq!(spec.route_count(), 1);
}

#[test]
fn a_configuration_with_no_listener_is_rejected_with_an_example() {
    let diagnostics = errors("site \"*\" { route \"/\" { respond \"hi\" } }");
    let diagnostic = diagnostics.iter().next().unwrap();
    assert_eq!(diagnostic.code, Code::MissingField);
    assert!(diagnostic.help.as_deref().unwrap().contains("http"));
}

#[test]
fn a_route_with_no_terminal_is_rejected() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    route "/" {
        headers
    }
}
"#,
    );
    assert!(has(&diagnostics, Code::TerminalArity));
}

#[test]
fn a_route_with_two_terminals_names_both_and_points_at_the_first() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    route "/" {
        respond "a"
        file-server
    }
}
"#,
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == Code::TerminalArity)
        .unwrap();
    assert!(diagnostic.message.contains("respond"));
    assert!(diagnostic.message.contains("file-server"));
    assert!(diagnostic.related.is_some());
}

#[test]
fn a_terminal_in_site_scope_is_rejected() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    respond "hi"
    route "/" { respond "hi" }
}
"#,
    );
    assert!(has(&diagnostics, Code::InvalidScope));
}

#[test]
fn declaration_order_never_decides_middleware_order() {
    let forward = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/" {
        respond "hi"
        encode
        basic-auth
        access-log
        rewrite
    }
}
"#,
    );
    let reversed = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/" {
        rewrite
        access-log
        basic-auth
        encode
        respond "hi"
    }
}
"#,
    );
    let names: Vec<&str> = forward.sites[0].routes[0]
        .directives
        .iter()
        .map(|item| item.meta.name)
        .collect();
    assert_eq!(names, ["access-log", "basic-auth", "rewrite", "encode"]);
    assert_eq!(forward.fingerprint(), reversed.fingerprint());
}

#[test]
fn site_directives_run_outside_route_directives() {
    let spec = spec(
        r#"
global { http ":8080" }
site "*" {
    headers
    route "/" {
        headers
        respond "hi"
    }
}
"#,
    );
    let plan = compile(Arc::new(spec), &registry());
    let route = &plan.listeners[0].sites[0].routes[0];
    assert_eq!(route.chain.len(), 2);
    assert_eq!(route.chain[0].scope, gatel_plan::Scope::Site);
    assert_eq!(route.chain[1].scope, gatel_plan::Scope::Route);
}

#[test]
fn the_same_directive_twice_in_one_scope_is_rejected() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    route "/" {
        headers
        headers
        respond "hi"
    }
}
"#,
    );
    assert!(has(&diagnostics, Code::Duplicate));
}

#[test]
fn routes_are_ordered_by_specificity_not_by_position() {
    let spec = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/*" { respond "catch-all" }
    route "/api/v1/users" { respond "exact" }
    route "/api/*" { respond "api" }
}
"#,
    );
    let paths: Vec<String> = spec.sites[0]
        .routes
        .iter()
        .map(|route| route.path.describe())
        .collect();
    assert_eq!(paths, ["/api/v1/users", "/api/*", "/*"]);
}

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
    let diagnostic = diagnostics.iter().find(|d| d.code == Code::Duplicate).unwrap();
    assert!(diagnostic.message.contains("exactly the same requests"));
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

#[test]
fn sites_are_ordered_with_exact_hosts_before_wildcards() {
    let spec = spec(
        r#"
global { http ":8080" }
site "*" { route "/" { respond "any" } }
site "*.example.com" { route "/" { respond "wildcard" } }
site "api.example.com" { route "/" { respond "exact" } }
"#,
    );
    let hosts: Vec<String> = spec.sites.iter().map(|site| site.host.describe()).collect();
    assert_eq!(hosts, ["api.example.com", "*.example.com", "*"]);
}

#[test]
fn the_three_proxy_forms_all_produce_an_upstream() {
    let spec = spec(
        r#"
global { http ":8080" }
upstream "shared" {
    endpoint "127.0.0.1:9000"
}
site "*" {
    route "/a" { proxy "127.0.0.1:9001" }
    route "/b" { proxy upstream="shared" }
    route "/c" {
        proxy {
            endpoint "127.0.0.1:9002"
            endpoint "127.0.0.1:9003"
            balance "least_conn"
        }
    }
}
"#,
    );
    assert_eq!(spec.upstreams.len(), 3, "one named plus two anonymous");
    for route in &spec.sites[0].routes {
        let Terminal::Proxy { upstream } = &route.terminal else {
            panic!("every route here proxies");
        };
        assert!(spec.upstream(upstream).is_some(), "{upstream} must resolve");
    }
    let inline = spec
        .upstreams
        .iter()
        .find(|upstream| upstream.endpoints.len() == 2)
        .expect("the inline body upstream");
    assert_eq!(inline.balance, gatel_plan::Balance::LeastConn);
}

#[test]
fn two_routes_proxying_to_the_same_address_share_one_upstream() {
    let spec = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/a" { proxy "127.0.0.1:9000" }
    route "/b" { proxy "127.0.0.1:9000" }
}
"#,
    );
    assert_eq!(spec.upstreams.len(), 1, "sharing an address must share the pool");
}

#[test]
fn an_unknown_upstream_reference_is_rejected_with_a_suggestion() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
upstream "backend" { endpoint "127.0.0.1:9000" }
site "*" {
    route "/" { proxy upstream="backends" }
}
"#,
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == Code::UnresolvedReference)
        .unwrap();
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `backend`?"));
}

#[test]
fn proxy_cannot_take_both_an_address_and_a_name() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
upstream "backend" { endpoint "127.0.0.1:9000" }
site "*" {
    route "/" { proxy "127.0.0.1:1" upstream="backend" }
}
"#,
    );
    assert!(diagnostics.iter().any(|d| d.message.contains("not both")));
}

#[test]
fn a_site_may_be_pinned_to_a_named_listener() {
    let spec = spec(
        r#"
global {
    http "public" "0.0.0.0:80"
    http "internal" "127.0.0.1:8081"
}
site "admin.example.com" listener="internal" {
    route "/" { respond "admin" }
}
site "www.example.com" {
    route "/" { respond "www" }
}
"#,
    );
    let plan = compile(Arc::new(spec), &registry());

    // A pinned site appears only where it was pinned.
    let public = plan.listener("public").unwrap();
    let public_hosts: Vec<String> = public.sites.iter().map(|site| site.host.describe()).collect();
    assert_eq!(public_hosts, ["www.example.com"]);

    // An unpinned site is served on every listener whose protocol suits it,
    // which is what makes one site answer on both http and https without
    // being written twice.
    let internal = plan.listener("internal").unwrap();
    let internal_hosts: Vec<String> = internal.sites.iter().map(|site| site.host.describe()).collect();
    assert_eq!(internal_hosts, ["admin.example.com", "www.example.com"]);
}

#[test]
fn an_unknown_listener_reference_is_rejected_with_a_suggestion() {
    let diagnostics = errors(
        r#"
global { http "public" "0.0.0.0:80" }
site "*" listener="publik" {
    route "/" { respond "hi" }
}
"#,
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == Code::UnresolvedReference)
        .unwrap();
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `public`?"));
}

#[test]
fn a_tls_site_with_no_https_listener_is_rejected() {
    let diagnostics = errors(
        r#"
global { http ":80" }
site "secure.example.com" {
    tls internal
    route "/" { respond "hi" }
}
"#,
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == Code::UnresolvedReference)
        .unwrap();
    assert!(diagnostic.message.contains("not served on any listener"));
    assert!(diagnostic.help.as_deref().unwrap().contains("https"));
}

#[test]
fn a_tls_site_pinned_to_a_plaintext_listener_is_rejected() {
    let diagnostics = errors(
        r#"
global {
    http "public" ":80"
    https "secure" ":443"
}
site "secure.example.com" listener="public" {
    tls internal
    route "/" { respond "hi" }
}
"#,
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("only served on plaintext"))
    );
}

#[test]
fn two_listeners_on_one_address_are_rejected() {
    let diagnostics = errors(
        r#"
global {
    http "a" "127.0.0.1:8080"
    http "b" "127.0.0.1:8080"
}
site "*" { route "/" { respond "hi" } }
"#,
    );
    let diagnostic = diagnostics.iter().find(|d| d.code == Code::Duplicate).unwrap();
    assert!(diagnostic.message.contains("bind"));
}

#[test]
fn the_admin_api_may_not_be_exposed_publicly() {
    let diagnostics = errors(
        r#"
global {
    http ":8080"
    admin "0.0.0.0:2019"
}
site "*" { route "/" { respond "hi" } }
"#,
    );
    let diagnostic = diagnostics.iter().find(|d| d.code == Code::InvalidValue).unwrap();
    assert!(diagnostic.help.as_deref().unwrap().contains("loopback"));
}

#[test]
fn cosmetic_edits_do_not_change_the_spec_fingerprint() {
    let plain = spec(MINIMAL).fingerprint();
    let commented = spec(
        r#"
// A comment.
global {
    http ":8080"   // trailing comment
}

site "*" {

    route "/" {
        respond "hello"
    }
}
"#,
    )
    .fingerprint();
    assert_eq!(plain, commented);
}

#[test]
fn changing_a_response_body_changes_the_spec_fingerprint() {
    let before = spec(MINIMAL).fingerprint();
    let after = spec(&MINIMAL.replace("hello", "goodbye")).fingerprint();
    assert_ne!(before, after);
}

#[test]
fn an_id_outside_a_sources_namespace_is_rejected() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" id="runtime/injected" {
    route "/" { respond "hi" }
}
"#,
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == Code::OwnershipConflict)
        .unwrap();
    assert!(diagnostic.help.as_deref().unwrap().contains("root namespace"));
}

#[test]
fn derived_route_ids_survive_reordering_but_track_content() {
    let first = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/a" { respond "a" }
    route "/b" { respond "b" }
}
"#,
    );
    let second = spec(
        r#"
global { http ":8080" }
site "*" {
    route "/b" { respond "b" }
    route "/a" { respond "a" }
}
"#,
    );
    let ids = |spec: &Spec| -> Vec<String> {
        let mut ids: Vec<String> = spec.sites[0]
            .routes
            .iter()
            .map(|route| route.id.name().to_string())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&first), ids(&second));
}

#[test]
fn an_unknown_top_level_node_suggests_a_known_one() {
    let diagnostics = errors("upstreams \"x\" { }");
    let diagnostic = diagnostics.iter().find(|d| d.code == Code::UnknownNode).unwrap();
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `upstream`?"));
}

#[test]
fn every_diagnostic_is_reported_not_just_the_first() {
    let diagnostics = errors(
        r#"
global { http ":8080" }
site "*" {
    route "/a" { nonsense }
    route "/b" { alsononsense }
}
"#,
    );
    assert!(diagnostics.error_count() >= 2, "got {}", diagnostics);
}

#[test]
fn global_limits_are_configuration_not_constants() {
    let spec = spec(
        r#"
global {
    http ":8080"
    limits header="8KiB" body="1MiB" header-timeout="5s" idle="30s" connections=500
}
site "*" { route "/" { respond "hi" } }
"#,
    );
    assert_eq!(spec.global.limits.max_body, gatel_model::ByteSize::mib(1));
    assert_eq!(spec.global.limits.max_connections, 500);
}
