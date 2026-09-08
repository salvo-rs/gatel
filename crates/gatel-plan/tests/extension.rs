//! What a bundle from outside this crate can do.
//!
//! The registry, the phases and the state model are extension points, and an
//! extension point is only one if something outside the crate can use it. These
//! tests are that something: they define a directive here, in a test, with no
//! help from the kernel beyond the traits it publishes.

use std::sync::Arc;

use gatel_model::{Diagnostics, Hasher, Node, Snapshot, SourceId, SourceRev, SourceSnapshot};
use gatel_plan::{Bundle, Directive, DirectiveConfig, DirectiveMeta, Phase, Registry, Scopes, compile};
use salvo_core::Handler;

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

#[test]
fn terminals_have_stable_route_specific_owners() {
    let registry = Registry::builder()
        .bundle(Bundle::new("fixture", "1").with(Stub(DirectiveMeta::terminal("answer"))))
        .build()
        .unwrap();
    let normalized = |other: &str| {
        gatel_plan::normalize(&snapshot(&format!(
        "global {{ http \":8080\"; }}\nsite \"*\" {{ route \"/a\" {{ answer; }}; route \"/{other}\" {{ answer; }}; }}"
    )), &registry).unwrap()
    };
    let spec = normalized("b");
    let owners = spec.sites[0]
        .routes
        .iter()
        .map(|route| {
            let gatel_plan::Terminal::Directive(terminal) = &route.terminal else {
                panic!("expected directive");
            };
            let expected = gatel_model::ResourceId::derived(
                gatel_model::ResourceKind::Directive,
                &[route.id.name(), "answer", "route"],
            );
            assert_eq!(terminal.owner, expected);
            terminal.owner.clone()
        })
        .collect::<Vec<_>>();
    assert_ne!(owners[0], owners[1]);
    let changed = normalized("c");
    let original = spec.sites[0]
        .routes
        .iter()
        .find(|r| r.path.describe() == "/a")
        .unwrap();
    let retained = changed.sites[0]
        .routes
        .iter()
        .find(|r| r.id == original.id)
        .unwrap();
    let (gatel_plan::Terminal::Directive(a), gatel_plan::Terminal::Directive(b)) =
        (&original.terminal, &retained.terminal)
    else {
        panic!("expected directives");
    };
    assert_eq!(a.owner, b.owner);
}

#[test]
fn external_cache_effect_cannot_bypass_a_guard_in_another_scope() {
    let make = |reverse| {
        let cache = DirectiveMeta::middleware("external-cache", Phase::Request, Scopes::BOTH)
            .with_effects(&[gatel_plan::Effect::CacheLookup]);
        Registry::builder()
            .bundle(
                Bundle::new("external", "1")
                    .with(Stub(if reverse { cache.before(&["guard"]) } else { cache }))
                    .with(Stub(DirectiveMeta::middleware(
                        "guard",
                        Phase::Guard,
                        Scopes::BOTH,
                    )))
                    .with(Stub(DirectiveMeta::terminal("answer"))),
            )
            .build()
            .unwrap()
    };
    let input =
        snapshot("global { http \":8080\" }\nsite \"*\" { external-cache; route \"/\" { guard; answer; }; }");
    let registry = make(false);
    let spec = gatel_plan::normalize(&input, &registry).unwrap();
    let chain = gatel_plan::ordering::order_route(
        spec.sites[0]
            .directives
            .iter()
            .chain(spec.sites[0].routes[0].directives.iter())
            .cloned()
            .collect(),
        &spec.sites[0].routes[0].terminal,
    )
    .unwrap();
    assert_eq!(chain[0].meta.name, "guard");
    assert_eq!(chain[1].meta.name, "external-cache");
    assert!(gatel_plan::normalize(&input, &make(true)).is_err());
}

#[test]
fn terminal_constraints_are_checked_during_normalization() {
    use gatel_plan::Effect;
    let middleware = || DirectiveMeta::middleware("guard", Phase::Guard, Scopes::BOTH);
    let terminal = || DirectiveMeta::terminal("answer");
    for (guard, answer, message) in [
        (middleware().conflicts_with(&["answer"]), terminal(), "conflicts"),
        (middleware(), terminal().conflicts_with(&["guard"]), "conflicts"),
        (middleware(), terminal().before(&["guard"]), "must execute last"),
        (
            middleware().with_effects(&[Effect::ReadRequestBody]),
            terminal().with_effects(&[Effect::RequestBodyLimit]),
            "must execute last",
        ),
    ] {
        let registry = Registry::builder()
            .bundle(Bundle::new("external", "1").with(Stub(guard)).with(Stub(answer)))
            .build()
            .unwrap();
        for text in [
            "global { http \":8080\" }\nsite \"*\" { guard; route \"/\" { answer; }; }",
            "global { http \":8080\" }\nsite \"*\" { route \"/\" { guard; answer; }; }",
        ] {
            let error = gatel_plan::normalize(&snapshot(text), &registry).unwrap_err();
            assert!(error.iter().any(|d| d.message.contains(message)), "{error}");
        }
    }
}

#[test]
fn middleware_can_precede_terminal_and_conflict_with_kernel_proxy() {
    let registry = Registry::builder()
        .bundle(
            Bundle::new("external", "1")
                .with(Stub(
                    DirectiveMeta::middleware("guard", Phase::Guard, Scopes::BOTH)
                        .before(&["answer"])
                        .conflicts_with(&["proxy"]),
                ))
                .with(Stub(DirectiveMeta::terminal("answer").before(&["absent"]))),
        )
        .build()
        .unwrap();
    let valid = snapshot("global { http \":8080\" }\nsite \"*\" { guard; route \"/\" { answer; }; }");
    let spec = gatel_plan::normalize(&valid, &registry).unwrap();
    let _plan = compile(Arc::new(spec), &registry);
    let invalid = snapshot(
        "global { http \":8080\" }\nsite \"*\" { guard; route \"/\" { proxy \"127.0.0.1:9000\"; }; }",
    );
    let error = gatel_plan::normalize(&invalid, &registry).unwrap_err();
    assert!(
        error
            .iter()
            .any(|d| d.message.contains("conflicts with terminal `proxy`")),
        "{error}"
    );
}

/// A stand-in terminal, so a route has one.
struct Stub(DirectiveMeta);

impl DirectiveConfig for StubConfig {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.0);
    }

    fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Err("not built in compiler tests".into())
    }

    fn describe(&self) -> String {
        self.0.clone()
    }
}

struct StubConfig(String);

impl Directive for Stub {
    fn meta(&self) -> &DirectiveMeta {
        &self.0
    }

    fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        Some(Arc::new(StubConfig(node.name.clone())))
    }
}

/// A directive that keeps something between requests, declared entirely from
/// outside `gatel-plan`.
///
/// This is the test that the extension point is one. While `StateKind` spelled
/// out `rate-limit` and `cache`, a directive in any other crate could not have
/// state at all without a patch to the kernel — which made "a directive
/// declares what it needs to keep" true only for the directives the kernel
/// already knew about.
struct Counting(DirectiveMeta);

struct CountingConfig(u32);

impl DirectiveConfig for CountingConfig {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_u64(self.0 as u64);
    }

    fn state(&self) -> Option<gatel_plan::DirectiveState> {
        let window = self.0;
        Some(gatel_plan::DirectiveState {
            compatibility: {
                let mut hasher = Hasher::new();
                hasher.update_u64(window as u64);
                hasher.finish()
            },
            create: Box::new(|| {
                Arc::new(std::sync::atomic::AtomicU64::new(0)) as Arc<dyn std::any::Any + Send + Sync>
            }),
        })
    }

    fn build(&self, _cx: gatel_plan::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        Err("not built in compiler tests".into())
    }

    fn describe(&self) -> String {
        format!("counting {}", self.0)
    }
}

impl Directive for Counting {
    fn meta(&self) -> &DirectiveMeta {
        &self.0
    }

    fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let window = node
            .prop("window")
            .and_then(|property| property.value.as_int())
            .unwrap_or(1) as u32;
        Some(Arc::new(CountingConfig(window)))
    }
}

fn registry_with_a_third_party_bundle() -> Registry {
    Registry::builder()
        .bundle(Bundle::new("std", "1").with(Stub(DirectiveMeta::terminal("respond"))))
        .bundle(
            Bundle::new("elsewhere", "1").with(Counting(DirectiveMeta::middleware(
                "counting",
                Phase::Guard,
                Scopes::BOTH,
            ))),
        )
        .build()
        .unwrap()
}

#[test]
fn a_directive_from_another_bundle_can_keep_state() {
    let text = r#"
global { http ":8080" }
site "*" {
    route "/" {
        counting window=5
        respond "hi"
    }
}
"#;
    let registry = registry_with_a_third_party_bundle();
    let spec = gatel_plan::normalize(&snapshot(text), &registry)
        .unwrap_or_else(|diagnostics| panic!("{diagnostics}"));
    let plan = compile(Arc::new(spec), &registry);

    let binding = plan
        .bindings
        .iter()
        .find(|binding| binding.kind == gatel_plan::StateKind::Directive("counting"))
        .expect("the directive's state is bound, without the kernel knowing what it is");
    // Owned by the route it was written on, like every other directive's.
    assert_eq!(binding.owner.kind(), gatel_model::ResourceKind::Directive);
}

#[test]
fn a_third_party_directives_state_is_named_after_it() {
    // Not chosen by the directive: two of them choosing the same name would
    // share a slot and downcast into each other's type.
    let registry = registry_with_a_third_party_bundle();
    let spec = gatel_plan::normalize(
        &snapshot(
            r#"
global { http ":8080" }
site "*" { route "/" { counting window=5
        respond "hi" } }
"#,
        ),
        &registry,
    )
    .unwrap();
    let plan = compile(Arc::new(spec), &registry);
    let kinds: Vec<&str> = plan
        .bindings
        .iter()
        .map(|binding| binding.kind.as_str())
        .collect();
    assert!(kinds.contains(&"counting"), "{kinds:?}");
}
