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
