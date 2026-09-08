//! Directives: the extension point for HTTP behaviour.
//!
//! A directive owns a node name in `site` or `route` scope, decodes it into a
//! typed configuration, and later builds a Salvo handler from that
//! configuration. The registry is built once at startup and is immutable, so
//! "unknown node" and "missing capability" are both decided during compilation
//! rather than when a request arrives.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use gatel_model::{Code, Diagnostic, Diagnostics, Hasher, Node, ResourceId, did_you_mean};
pub use gatel_plugin_api::{DirectiveMeta, Effect, FailurePolicy, Phase, Role, Scope, Scopes};
#[cfg(test)]
use salvo_core::Handler;

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Site => "site",
        Scope::Route => "route",
    }
}

pub use gatel_plugin_runtime::{BuildContext, Bundle, Directive, DirectiveConfig, DirectiveState};

/// A directive as it appears at one place in the configuration.
#[derive(Clone)]
pub struct DirectiveInstance {
    pub meta: DirectiveMeta,
    pub config: Arc<dyn DirectiveConfig>,
    pub scope: Scope,
    /// Explicit `order=` if written, otherwise the directive's default.
    pub order: i32,
    /// Who owns this instance's state. Derived from where it sits, so moving a
    /// directive to another route gives it different state, and editing an
    /// unrelated part of the file does not.
    pub owner: ResourceId,
    pub origin: gatel_model::Origin,
}

impl DirectiveInstance {
    /// Give this instance an owner derived from the resource it sits on.
    pub fn own(&mut self, resource: &ResourceId) {
        self.owner = ResourceId::derived(
            gatel_model::ResourceKind::Directive,
            &[resource.name(), self.meta.name, scope_name(self.scope)],
        );
    }

    /// The state binding this instance needs, if any.
    pub fn state_binding(&self) -> Option<crate::plan::StateBinding> {
        self.config.state().map(|state| crate::plan::StateBinding {
            owner: self.owner.clone(),
            // Named after the directive rather than chosen by it. A directive
            // that named its own kind could collide with another's, and there
            // is nothing it could usefully say that its name does not.
            kind: crate::plan::StateKind::Directive(self.meta.name),
            compatibility: state.compatibility,
            policy: crate::plan::ResetPolicy::Preserve,
        })
    }

    /// The stable sort key: outer scope first, then phase, then order, then
    /// name. Declaration position never appears.
    pub fn sort_key(&self) -> (u8, u8, i32, &'static str) {
        let scope_rank = match self.scope {
            Scope::Site => 0,
            Scope::Route => 1,
        };
        let phase_rank = self.meta.role.phase().map(|phase| phase as u8).unwrap_or(u8::MAX);
        (scope_rank, phase_rank, self.order, self.meta.name)
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        self.meta.hash_into(hasher);
        hasher.update_u64(match self.scope {
            Scope::Site => 0,
            Scope::Route => 1,
        });
        hasher.update_i64(self.order as i64);
        self.config.hash_into(hasher);
    }
}

impl fmt::Debug for DirectiveInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectiveInstance")
            .field("name", &self.meta.name)
            .field("scope", &self.scope)
            .field("order", &self.order)
            .finish()
    }
}

/// The immutable set of directives this binary contains.
pub struct Registry {
    directives: BTreeMap<&'static str, Arc<dyn Directive>>,
    bundles: Vec<(&'static str, &'static str)>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("bundles", &self.bundles)
            .field("directives", &self.directives.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Registry {
    pub fn builder() -> RegistryBuilder {
        RegistryBuilder {
            directives: BTreeMap::new(),
            bundles: Vec::new(),
            conflicts: Vec::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Directive>> {
        self.directives.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.directives.keys().copied()
    }

    pub fn bundles(&self) -> &[(&'static str, &'static str)] {
        &self.bundles
    }

    pub fn len(&self) -> usize {
        self.directives.len()
    }

    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }

    /// Identifies exactly which capabilities this binary was built with, so a
    /// plan built by a differently-trimmed binary is never mistaken for it.
    pub fn build_hash(&self) -> gatel_model::Fingerprint {
        let mut hasher = Hasher::new();
        for (name, bundle_version) in &self.bundles {
            hasher.update_str(name);
            hasher.update_str(bundle_version);
        }
        for directive in self.directives.values() {
            directive.meta().hash_into(&mut hasher);
        }
        hasher.finish()
    }

    /// Decode a node in a scope, reporting an unknown name, a wrong scope or a
    /// capability this build does not have.
    pub fn decode(
        &self,
        node: &Node,
        scope: Scope,
        diagnostics: &mut Diagnostics,
    ) -> Option<DirectiveInstance> {
        let Some(directive) = self.directives.get(node.name.as_str()) else {
            let mut diagnostic = Diagnostic::error(
                Code::UnknownNode,
                node.origin.clone(),
                format!("unknown directive `{}`", node.name),
            );
            if let Some(suggestion) = did_you_mean(&node.name, self.names_for(scope)) {
                diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
            } else {
                diagnostic = diagnostic
                    .with_help("run `gatel capabilities` to see what this build supports".to_string());
            }
            diagnostics.push(diagnostic);
            return None;
        };

        let meta = directive.meta().clone();
        if !meta.scopes.allows(scope) {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidScope,
                    node.origin.clone(),
                    format!("`{}` cannot appear in a {scope}", meta.name),
                )
                .with_help(format!(
                    "`{}` belongs in a {}",
                    meta.name,
                    meta.scopes.describe()
                )),
            );
            return None;
        }

        // `order` is read here rather than by the directive, so every directive
        // gets consistent ordering without opting in.
        if node
            .props
            .iter()
            .filter(|property| property.name == "order")
            .count()
            > 1
        {
            diagnostics.error(
                Code::InvalidValue,
                node.origin.clone(),
                "`order` must not be repeated",
            );
            return None;
        }
        let order = match node.prop("order") {
            Some(property) => match property.value.as_int() {
                Some(value) => match i32::try_from(value) {
                    Ok(value) => value,
                    Err(_) => {
                        diagnostics.error(
                            Code::InvalidValue,
                            property.origin.clone(),
                            "`order` must fit a signed 32-bit integer",
                        );
                        return None;
                    }
                },
                None => {
                    diagnostics.error(
                        Code::InvalidValue,
                        property.origin.clone(),
                        "`order` must be an integer",
                    );
                    return None;
                }
            },
            None => meta.order,
        };
        let mut stripped = node.clone();
        stripped.props.retain(|property| property.name != "order");

        let config = directive.decode(&stripped, diagnostics)?;
        // A placeholder until the enclosing resource is known; `normalize`
        // calls `own` once the site or route it belongs to has an identity.
        let owner = ResourceId::derived(
            gatel_model::ResourceKind::Directive,
            &[meta.name, scope_name(scope)],
        );
        Some(DirectiveInstance {
            meta,
            config,
            scope,
            order,
            owner,
            origin: node.origin.clone(),
        })
    }

    fn names_for(&self, scope: Scope) -> Vec<&'static str> {
        self.directives
            .iter()
            .filter(|(_, directive)| directive.meta().scopes.allows(scope))
            .map(|(name, _)| *name)
            .collect()
    }
}

pub struct RegistryBuilder {
    directives: BTreeMap<&'static str, Arc<dyn Directive>>,
    bundles: Vec<(&'static str, &'static str)>,
    conflicts: Vec<String>,
}

impl RegistryBuilder {
    pub fn bundle(mut self, bundle: Bundle) -> Self {
        if self.bundles.iter().any(|(name, _)| *name == bundle.name) {
            self.conflicts
                .push(format!("bundle `{}` is registered more than once", bundle.name));
        }
        self.bundles.push((bundle.name, bundle.version));
        for directive in bundle.directives {
            let meta = directive.meta();
            if meta.failure_policy == FailurePolicy::Open && meta.role != Role::Middleware(Phase::Observe) {
                self.conflicts
                    .push(format!("`{}` declares fail-open outside observation", meta.name));
            }
            let name = directive.meta().name;
            if self.directives.insert(name, directive).is_some() {
                self.conflicts
                    .push(format!("`{name}` is claimed by more than one bundle"));
            }
        }
        self
    }

    /// Fails rather than silently letting one bundle shadow another, because a
    /// shadowed directive would change behaviour with no diagnostic anywhere.
    pub fn build(mut self) -> Result<Registry, String> {
        if !self.conflicts.is_empty() {
            return Err(self.conflicts.join("; "));
        }
        // Registration order is a host implementation detail, not behaviour.
        self.bundles.sort_unstable();
        Ok(Registry {
            directives: self.directives,
            bundles: self.bundles,
        })
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    struct Noop(DirectiveMeta);

    struct NoopConfig {
        label: String,
    }

    impl DirectiveConfig for NoopConfig {
        fn hash_into(&self, hasher: &mut Hasher) {
            hasher.update_str(&self.label);
        }

        fn build(&self, _cx: crate::BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
            Err("not built in tests".to_string())
        }

        fn describe(&self) -> String {
            self.label.clone()
        }
    }

    impl Directive for Noop {
        fn meta(&self) -> &DirectiveMeta {
            &self.0
        }

        fn decode(&self, node: &Node, _: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
            Some(Arc::new(NoopConfig {
                label: node.name.clone(),
            }))
        }
    }

    fn registry() -> Registry {
        Registry::builder()
            .bundle(
                Bundle::new("test", "1")
                    .with(Noop(DirectiveMeta::middleware(
                        "headers",
                        Phase::Response,
                        Scopes::BOTH,
                    )))
                    .with(Noop(DirectiveMeta::middleware(
                        "basic-auth",
                        Phase::Guard,
                        Scopes::ROUTE,
                    )))
                    .with(Noop(DirectiveMeta::terminal("respond"))),
            )
            .build()
            .unwrap()
    }

    fn node(name: &str) -> Node {
        Node::new(name, Origin::synthetic("test"))
    }

    #[test]
    fn an_unknown_directive_suggests_a_known_one() {
        let mut diagnostics = Diagnostics::new();
        assert!(
            registry()
                .decode(&node("header"), Scope::Route, &mut diagnostics)
                .is_none()
        );
        let diagnostic = diagnostics.iter().next().unwrap();
        assert_eq!(diagnostic.code, Code::UnknownNode);
        assert_eq!(diagnostic.help.as_deref(), Some("did you mean `headers`?"));
    }

    #[test]
    fn a_directive_in_the_wrong_scope_is_rejected() {
        let mut diagnostics = Diagnostics::new();
        assert!(
            registry()
                .decode(&node("basic-auth"), Scope::Site, &mut diagnostics)
                .is_none()
        );
        let diagnostic = diagnostics.iter().next().unwrap();
        assert_eq!(diagnostic.code, Code::InvalidScope);
        assert!(diagnostic.help.as_deref().unwrap().contains("belongs in a route"));
    }

    #[test]
    fn writing_a_guard_after_a_terminal_does_not_move_it() {
        let registry = registry();
        let mut diagnostics = Diagnostics::new();
        let auth = registry
            .decode(&node("basic-auth"), Scope::Route, &mut diagnostics)
            .unwrap();
        let headers = registry
            .decode(&node("headers"), Scope::Route, &mut diagnostics)
            .unwrap();
        let respond = registry
            .decode(&node("respond"), Scope::Route, &mut diagnostics)
            .unwrap();
        let mut instances = [respond, headers, auth];
        instances.sort_by_key(DirectiveInstance::sort_key);
        let order: Vec<&str> = instances.iter().map(|item| item.meta.name).collect();
        assert_eq!(order, ["basic-auth", "headers", "respond"]);
    }

    #[test]
    fn site_scope_sorts_before_route_scope() {
        let registry = registry();
        let mut diagnostics = Diagnostics::new();
        let site = registry
            .decode(&node("headers"), Scope::Site, &mut diagnostics)
            .unwrap();
        let route = registry
            .decode(&node("headers"), Scope::Route, &mut diagnostics)
            .unwrap();
        assert!(site.sort_key() < route.sort_key());
    }

    #[test]
    fn explicit_order_overrides_the_default_within_a_phase() {
        let registry = registry();
        let mut diagnostics = Diagnostics::new();
        let first = registry
            .decode(
                &node("headers").with_prop("order", Value::int(-5, Origin::synthetic("t"))),
                Scope::Route,
                &mut diagnostics,
            )
            .unwrap();
        let second = registry
            .decode(&node("headers"), Scope::Route, &mut diagnostics)
            .unwrap();
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        assert_eq!(first.order, -5);
        assert!(first.sort_key() < second.sort_key());
    }

    #[test]
    fn two_bundles_claiming_one_name_fail_the_build() {
        let error = Registry::builder()
            .bundle(Bundle::new("a", "1").with(Noop(DirectiveMeta::terminal("respond"))))
            .bundle(Bundle::new("b", "1").with(Noop(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap_err();
        assert!(error.contains("more than one bundle"));
    }

    #[test]
    fn the_build_hash_changes_when_a_bundle_is_trimmed() {
        let full = registry().build_hash();
        let trimmed = Registry::builder()
            .bundle(Bundle::new("test", "1").with(Noop(DirectiveMeta::terminal("respond"))))
            .build()
            .unwrap()
            .build_hash();
        assert_ne!(full, trimmed);
    }

    #[test]
    fn bundle_registration_order_does_not_change_build_identity() {
        let one = || Bundle::new("one", "1").with(Noop(DirectiveMeta::terminal("respond")));
        let two = || Bundle::new("two", "1").with(Noop(DirectiveMeta::terminal("redirect")));
        let a = Registry::builder().bundle(one()).bundle(two()).build().unwrap();
        let b = Registry::builder().bundle(two()).bundle(one()).build().unwrap();
        assert_eq!(a.build_hash(), b.build_hash());
        assert_eq!(a.bundles(), b.bundles());
    }

    #[test]
    fn conflicting_bundle_identity_is_rejected_even_without_directive_overlap() {
        let error = Registry::builder()
            .bundle(Bundle::new("same", "1").with(Noop(DirectiveMeta::terminal("respond"))))
            .bundle(Bundle::new("same", "2").with(Noop(DirectiveMeta::terminal("redirect"))))
            .build()
            .unwrap_err();
        assert!(error.contains("bundle `same` is registered more than once"));
    }

    #[test]
    fn changing_allowed_scopes_changes_build_identity() {
        let make = |scopes| {
            Registry::builder()
                .bundle(Bundle::new("test", "1").with(Noop(DirectiveMeta::middleware(
                    "headers",
                    Phase::Response,
                    scopes,
                ))))
                .build()
                .unwrap()
                .build_hash()
        };
        assert_ne!(make(Scopes::SITE), make(Scopes::ROUTE));
        assert_ne!(make(Scopes::BOTH), make(Scopes::ROUTE));
    }

    #[test]
    fn out_of_range_order_is_rejected_instead_of_wrapping() {
        for value in [i64::MIN, i32::MIN as i64 - 1, i32::MAX as i64 + 1, i64::MAX] {
            let mut diagnostics = Diagnostics::new();
            let result = registry().decode(
                &node("headers").with_prop("order", Value::int(value, Origin::synthetic("test"))),
                Scope::Route,
                &mut diagnostics,
            );
            assert!(result.is_none());
            assert_eq!(diagnostics.iter().next().unwrap().code, Code::InvalidValue);
        }
    }

    #[test]
    fn a_security_directive_cannot_register_as_fail_open() {
        let error = Registry::builder()
            .bundle(Bundle::new("unsafe", "1").with(Noop(
                DirectiveMeta::middleware("authentication", Phase::Guard, Scopes::BOTH).fail_open(),
            )))
            .build()
            .unwrap_err();
        assert!(error.contains("fail-open outside observation"));
    }

    #[test]
    fn duplicate_order_is_not_silently_removed_before_decode() {
        let mut duplicate = node("headers");
        duplicate = duplicate.with_prop("order", Value::int(1, Origin::synthetic("test")));
        let mut second = duplicate.props[0].clone();
        second.value = Value::int(2, Origin::synthetic("test"));
        duplicate.props.push(second);
        let mut diagnostics = Diagnostics::new();
        assert!(
            registry()
                .decode(&duplicate, Scope::Route, &mut diagnostics)
                .is_none()
        );
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .message
                .contains("must not be repeated")
        );
    }

    fn composition(metas: Vec<DirectiveMeta>) -> Vec<DirectiveInstance> {
        let mut builder = Bundle::new("composition", "1");
        for meta in metas {
            builder = builder.with(Noop(meta));
        }
        let registry = Registry::builder().bundle(builder).build().unwrap();
        registry
            .names()
            .map(|name| {
                registry
                    .decode(&node(name), Scope::Route, &mut Diagnostics::new())
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn required_order_overrides_user_order_and_outer_scope() {
        let mut chain = composition(vec![
            DirectiveMeta::middleware("guard", Phase::Guard, Scopes::BOTH).before(&["consumer"]),
            DirectiveMeta::middleware("consumer", Phase::Guard, Scopes::BOTH),
        ]);
        for item in &mut chain {
            if item.meta.name == "consumer" {
                item.scope = Scope::Site;
                item.order = -999;
            }
        }
        let ordered = crate::ordering::order_chain(chain).unwrap();
        assert_eq!(
            ordered.iter().map(|item| item.meta.name).collect::<Vec<_>>(),
            ["guard", "consumer"]
        );
    }

    #[test]
    fn cycles_and_conflicting_plugins_are_rejected() {
        let a = || DirectiveMeta::middleware("a", Phase::Guard, Scopes::BOTH);
        let b = || DirectiveMeta::middleware("b", Phase::Guard, Scopes::BOTH);
        assert!(
            crate::ordering::order_chain(composition(vec![a().before(&["b"]), b().before(&["a"])])).is_err()
        );
        assert!(crate::ordering::order_chain(composition(vec![a().conflicts_with(&["b"]), b()])).is_err());
    }

    #[test]
    fn requirements_for_absent_optional_plugins_do_not_fail() {
        let chain = composition(vec![
            DirectiveMeta::middleware("guard", Phase::Guard, Scopes::BOTH).before(&["absent"]),
        ]);
        assert_eq!(crate::ordering::order_chain(chain).unwrap().len(), 1);
    }

    #[test]
    fn effect_rules_apply_to_external_names_across_scopes() {
        for effects in [
            &[Effect::ReadRequestBody][..],
            &[Effect::CallUpstream][..],
            &[Effect::ConsumeQuota][..],
        ] {
            let mut chain = composition(vec![
                DirectiveMeta::middleware("external-budget", Phase::Guard, Scopes::BOTH)
                    .with_effects(&[Effect::RequestBodyLimit]),
                DirectiveMeta::middleware("external-work", Phase::Guard, Scopes::BOTH).with_effects(effects),
            ]);
            for instance in &mut chain {
                if instance.meta.name == "external-work" {
                    instance.scope = Scope::Site;
                    instance.order = i32::MIN;
                } else {
                    instance.order = i32::MAX;
                }
            }
            let chain = crate::ordering::order_chain(chain).unwrap();
            assert_eq!(chain[0].meta.name, "external-budget");
            assert_eq!(chain[1].meta.name, "external-work");
        }
    }

    #[test]
    fn effects_cannot_be_overridden_by_explicit_constraints_or_phase() {
        let budget = || {
            DirectiveMeta::middleware("budget", Phase::Guard, Scopes::BOTH)
                .with_effects(&[Effect::RequestBodyLimit])
        };
        let reader = || {
            DirectiveMeta::middleware("reader", Phase::Guard, Scopes::BOTH)
                .with_effects(&[Effect::ReadRequestBody])
        };
        assert!(
            crate::ordering::order_chain(composition(vec![budget(), reader().before(&["budget"]),])).is_err()
        );
        let mut late = budget();
        late.role = Role::Middleware(Phase::Response);
        assert!(crate::ordering::order_chain(composition(vec![late, reader()])).is_err());
        // Two composite limit/read middleware cannot both protect each other's reads.
        assert!(
            crate::ordering::order_chain(composition(vec![
                budget().with_effects(&[Effect::RequestBodyLimit, Effect::ReadRequestBody]),
                reader().with_effects(&[Effect::RequestBodyLimit, Effect::ReadRequestBody]),
            ]))
            .is_err()
        );
    }

    #[test]
    fn effect_identity_is_canonical_and_behavior_sensitive() {
        let hash = |effects| {
            let mut hasher = Hasher::new();
            DirectiveMeta::terminal("test")
                .with_effects(effects)
                .hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(
            hash(&[Effect::CallUpstream, Effect::ConsumeQuota]),
            hash(&[Effect::ConsumeQuota, Effect::CallUpstream, Effect::ConsumeQuota]),
        );
        assert_ne!(hash(&[]), hash(&[Effect::CallUpstream]));
        assert_ne!(hash(&[Effect::ReadRequestBody]), hash(&[Effect::ConsumeQuota]));
    }
}
