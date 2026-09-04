//! Directives: the extension point for HTTP behaviour.
//!
//! A directive owns a node name in `site` or `route` scope, decodes it into a
//! typed configuration, and later builds a Salvo handler from that
//! configuration. The registry is built once at startup and is immutable, so
//! "unknown node" and "missing capability" are both decided during compilation
//! rather than when a request arrives.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use gatel_model::{Code, Diagnostic, Diagnostics, Fingerprint, Hasher, Node, ResourceId, did_you_mean};
use salvo_core::Handler;

use crate::plan::StateKind;

/// Where a middleware sits in the request chain.
///
/// The order is fixed by the enum, not by where the operator wrote the node:
/// putting `basic-auth` after `proxy` in the file cannot move authentication
/// after the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Outermost. Sees every request and the final response. May fail open.
    Observe,
    /// Rejections happen here: authentication, rate limits, filters, limits.
    Guard,
    /// Request rewriting.
    Request,
    /// Innermost, closest to the terminal. Response transformation.
    Response,
}

impl Phase {
    pub const ALL: [Phase; 4] = [Phase::Observe, Phase::Guard, Phase::Request, Phase::Response];

    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Observe => "observe",
            Phase::Guard => "guard",
            Phase::Request => "request",
            Phase::Response => "response",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a directive contributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Middleware(Phase),
    /// A route's single terminal. Exactly one per route.
    Terminal,
}

impl Role {
    pub fn phase(self) -> Option<Phase> {
        match self {
            Role::Middleware(phase) => Some(phase),
            Role::Terminal => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Role::Terminal)
    }
}

/// Which scopes a directive may appear in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scopes {
    pub site: bool,
    pub route: bool,
}

impl Scopes {
    pub const SITE: Self = Self {
        site: true,
        route: false,
    };
    pub const ROUTE: Self = Self {
        site: false,
        route: true,
    };
    pub const BOTH: Self = Self {
        site: true,
        route: true,
    };

    pub fn allows(self, scope: Scope) -> bool {
        match scope {
            Scope::Site => self.site,
            Scope::Route => self.route,
        }
    }

    pub fn describe(self) -> &'static str {
        match (self.site, self.route) {
            (true, true) => "site or route",
            (true, false) => "site",
            (false, true) => "route",
            (false, false) => "nowhere",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Site,
    Route,
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Site => "site",
        Scope::Route => "route",
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::Site => "site",
            Scope::Route => "route",
        })
    }
}

/// What happens when this directive fails at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    /// The request fails. The default, and the only option for anything that
    /// affects authorization, routing or the response body.
    Closed,
    /// The request continues. Only observation may declare this.
    Open,
}

/// Everything about a directive that does not depend on its configuration.
#[derive(Debug, Clone)]
pub struct DirectiveMeta {
    /// The node name claimed in KDL.
    pub name: &'static str,
    pub scopes: Scopes,
    pub role: Role,
    /// Default ordering within a phase; a node may override it with `order=`.
    pub order: i32,
    /// Bumped when the compiled behaviour changes, so a plan built by an older
    /// binary is never mistaken for an equivalent one.
    pub version: &'static str,
    /// The capability this directive provides, used by `gatel capabilities`
    /// and by the missing-capability diagnostic.
    pub capability: &'static str,
    pub failure_policy: FailurePolicy,
}

impl DirectiveMeta {
    pub const fn middleware(name: &'static str, phase: Phase, scopes: Scopes) -> Self {
        Self {
            name,
            scopes,
            role: Role::Middleware(phase),
            order: 0,
            version: "1",
            capability: name,
            failure_policy: FailurePolicy::Closed,
        }
    }

    pub const fn terminal(name: &'static str) -> Self {
        Self {
            name,
            scopes: Scopes::ROUTE,
            role: Role::Terminal,
            order: 0,
            version: "1",
            capability: name,
            failure_policy: FailurePolicy::Closed,
        }
    }

    pub const fn with_version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    /// Move this directive within its phase.
    ///
    /// Lower runs first. Needed only where alphabetical order would put a
    /// directive in the wrong place — a timeout that has to wrap the other
    /// guards rather than sit among them.
    pub const fn with_order(mut self, order: i32) -> Self {
        self.order = order;
        self
    }

    pub const fn fail_open(mut self) -> Self {
        self.failure_policy = FailurePolicy::Open;
        self
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.name);
        hasher.update_str(self.version);
        hasher.update_str(self.capability);
        hasher.update_u64(match self.role {
            Role::Middleware(phase) => phase as u64,
            Role::Terminal => 100,
        });
        hasher.update_i64(self.order as i64);
        hasher.update_bool(self.failure_policy == FailurePolicy::Open);
    }
}

/// State a directive keeps between requests.
///
/// A directive that needs one declares it here rather than reaching for a
/// global: a rate limiter's buckets must survive a reload that did not change
/// the limit, and must *not* survive one that did.
pub struct DirectiveState {
    pub kind: StateKind,
    /// Changes exactly when the existing buckets or counters stop meaning what
    /// they meant. Raising a window does; renaming an unrelated route does not.
    pub compatibility: Fingerprint,
    /// Builds a fresh instance when the existing one cannot be reused.
    pub create: Box<dyn Fn() -> Arc<dyn Any + Send + Sync> + Send + Sync>,
}

impl std::fmt::Debug for DirectiveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveState")
            .field("kind", &self.kind)
            .field("compatibility", &self.compatibility.short())
            .finish()
    }
}

/// What a directive is given when its handler is built.
///
/// Everything here is decided before anything is published, so a secret that
/// cannot be read or a path that does not exist stops a reload rather than
/// failing a request later.
pub struct BuildContext<'a> {
    /// The directory `file:` references resolve against.
    pub root: &'a std::path::Path,
    /// Whatever [`DirectiveConfig::state`] asked for, resolved against what is
    /// already running. `None` for a directive that asked for nothing.
    pub state: Option<Arc<dyn Any + Send + Sync>>,
    /// What the process measures and reports about itself. Process-wide and
    /// older than any generation, like a socket: a directive that exposes it
    /// captures the handle, it is never rebuilt for a reload.
    pub telemetry: &'a Arc<gatel_observe::Telemetry>,
}

impl BuildContext<'_> {
    /// Read the material a reference names, with the usual bounds and
    /// permission checks.
    pub fn secret(&self, reference: &gatel_model::SecretRef) -> Result<gatel_model::SecretString, String> {
        gatel_model::resolve_secret(reference, self.root)
    }

    /// Take the state as a concrete type, or say what went wrong.
    pub fn state<T: Any + Send + Sync>(&self, what: &str) -> Result<Arc<T>, String> {
        self.state
            .clone()
            .ok_or_else(|| format!("{what} was not provided"))?
            .downcast::<T>()
            .map_err(|_| format!("{what} is not of the expected type"))
    }
}

/// A decoded directive configuration.
///
/// `hash_into` must cover everything that changes behaviour and nothing that
/// does not, because it is what decides whether a reload is a no-op.
pub trait DirectiveConfig: Any + Send + Sync + 'static {
    fn hash_into(&self, hasher: &mut Hasher);

    /// Build the handler for one generation. Called during `prepare`, so a
    /// failure here rejects the candidate and leaves the running configuration
    /// untouched.
    ///
    /// Anything it needs from outside — a secret, a resolved path, its own
    /// state — arrives in the context.
    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String>;

    /// What this directive needs to keep between requests, if anything.
    fn state(&self) -> Option<DirectiveState> {
        None
    }

    /// One line for `gatel explain`.
    fn describe(&self) -> String;
}

/// A directive implementation: metadata plus a decoder.
pub trait Directive: Send + Sync + 'static {
    fn meta(&self) -> &DirectiveMeta;

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>>;
}

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
            kind: state.kind,
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

/// A named group of directives, shipped and trimmed as a unit.
pub struct Bundle {
    pub name: &'static str,
    pub version: &'static str,
    pub directives: Vec<Arc<dyn Directive>>,
}

impl Bundle {
    pub fn new(name: &'static str, version: &'static str) -> Self {
        Self {
            name,
            version,
            directives: Vec::new(),
        }
    }

    pub fn with(mut self, directive: impl Directive) -> Self {
        self.directives.push(Arc::new(directive));
        self
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
        let order = match node.prop("order") {
            Some(property) => match property.value.as_int() {
                Some(value) => value as i32,
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
        self.bundles.push((bundle.name, bundle.version));
        for directive in bundle.directives {
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
    pub fn build(self) -> Result<Registry, String> {
        if !self.conflicts.is_empty() {
            return Err(self.conflicts.join("; "));
        }
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
}
