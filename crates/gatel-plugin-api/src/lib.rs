//! Pure plugin contracts. No HTTP runtime, compiler or I/O dependencies.
#![forbid(unsafe_code)]
use std::fmt;

use gatel_model::Hasher;
mod effects;
pub use effects::Effect;

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
    /// Request-side behavior used to derive name-independent composition rules.
    pub effects: &'static [Effect],
    /// Directives this contribution must execute before, when present.
    pub before: &'static [&'static str],
    /// Directives that cannot safely share an execution chain with this one.
    pub conflicts: &'static [&'static str],
    /// The node name claimed in KDL.
    pub name: &'static str,
    pub scopes: Scopes,
    pub role: Role,
    /// Default ordering within a phase; a node may override it with `order=`.
    pub order: i32,
    /// Bumped when the compiled behaviour changes, so a plan built by an older
    /// binary is never mistaken for an equivalent one.
    pub version: &'static str,
    pub failure_policy: FailurePolicy,
}

impl DirectiveMeta {
    pub const fn middleware(name: &'static str, phase: Phase, scopes: Scopes) -> Self {
        Self {
            name,
            effects: &[],
            before: &[],
            conflicts: &[],
            scopes,
            role: Role::Middleware(phase),
            order: 0,
            version: "1",
            failure_policy: FailurePolicy::Closed,
        }
    }

    pub const fn terminal(name: &'static str) -> Self {
        Self {
            name,
            effects: &[],
            before: &[],
            conflicts: &[],
            scopes: Scopes::ROUTE,
            role: Role::Terminal,
            order: 0,
            version: "1",
            failure_policy: FailurePolicy::Closed,
        }
    }

    pub const fn with_version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    pub const fn with_effects(mut self, effects: &'static [Effect]) -> Self {
        self.effects = effects;
        self
    }

    pub const fn before(mut self, names: &'static [&'static str]) -> Self {
        self.before = names;
        self
    }

    pub const fn conflicts_with(mut self, names: &'static [&'static str]) -> Self {
        self.conflicts = names;
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
        effects::hash(self.effects, hasher);
        for names in [self.before, self.conflicts] {
            let mut names = names.to_vec();
            names.sort_unstable();
            names.dedup();
            hasher.update_u64(names.len() as u64);
            for name in names {
                hasher.update_str(name);
            }
        }
        hasher.update_str(self.name);
        hasher.update_str(self.version);
        hasher.update_bool(self.scopes.site);
        hasher.update_bool(self.scopes.route);
        hasher.update_u64(match self.role {
            Role::Middleware(phase) => phase as u64,
            Role::Terminal => 100,
        });
        hasher.update_i64(self.order as i64);
        hasher.update_bool(self.failure_policy == FailurePolicy::Open);
    }
}
