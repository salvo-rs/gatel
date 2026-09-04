//! Stable identities for configuration sources and resources.

use std::fmt;
use std::sync::Arc;

use crate::hash::{Fingerprint, Hasher};

/// Which frontend owns a resource. A source owns a namespace and may only
/// create, modify or delete resources inside it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId {
    kind: SourceKind,
    name: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    /// The KDL file tree. Owns the root namespace.
    File,
    /// The Admin API. Owns `runtime/`.
    Admin,
    /// A Kubernetes controller. Owns `k8s/<namespace>/`.
    Kubernetes,
}

impl SourceId {
    pub fn file(name: impl Into<Arc<str>>) -> Self {
        Self {
            kind: SourceKind::File,
            name: name.into(),
        }
    }

    pub fn admin() -> Self {
        Self {
            kind: SourceKind::Admin,
            name: Arc::from("runtime"),
        }
    }

    pub fn kubernetes(namespace: impl Into<Arc<str>>) -> Self {
        Self {
            kind: SourceKind::Kubernetes,
            name: namespace.into(),
        }
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The namespace prefix every resource this source owns must carry.
    /// The file source owns the root namespace and so has no prefix.
    pub fn namespace(&self) -> Option<String> {
        match self.kind {
            SourceKind::File => None,
            SourceKind::Admin => Some("runtime/".to_string()),
            SourceKind::Kubernetes => Some(format!("k8s/{}/", self.name)),
        }
    }

    /// Whether this source is allowed to own a resource with this name.
    pub fn owns(&self, resource_name: &str) -> bool {
        match self.namespace() {
            Some(prefix) => resource_name.starts_with(&prefix),
            None => !resource_name.contains('/'),
        }
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            SourceKind::File => write!(f, "file:{}", self.name),
            SourceKind::Admin => f.write_str("admin"),
            SourceKind::Kubernetes => write!(f, "k8s:{}", self.name),
        }
    }
}

/// The version a source was observed at. Comparing these is how a candidate
/// notices the world moved while it was compiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SourceRev(pub u64);

impl SourceRev {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SourceRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    Listener,
    Site,
    Route,
    Upstream,
    TlsPolicy,
    /// One directive at one place in the configuration. It is a resource
    /// because it can own state, and state needs an owner that survives a
    /// reload.
    Directive,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 6] = [
        ResourceKind::Listener,
        ResourceKind::Site,
        ResourceKind::Route,
        ResourceKind::Upstream,
        ResourceKind::TlsPolicy,
        ResourceKind::Directive,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Listener => "listener",
            ResourceKind::Site => "site",
            ResourceKind::Route => "route",
            ResourceKind::Upstream => "upstream",
            ResourceKind::TlsPolicy => "tls-policy",
            ResourceKind::Directive => "directive",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resource's logical identity, stable across reloads.
///
/// Names are either written by the operator (`id="api"`) or derived
/// deterministically from the resource's own content — never from its position
/// in the file, so reordering declarations never resets state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId {
    kind: ResourceKind,
    name: Arc<str>,
}

impl ResourceId {
    pub fn new(kind: ResourceKind, name: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }

    /// Derive a name from content when the operator did not supply one.
    pub fn derived(kind: ResourceKind, parts: &[&str]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update_str(kind.as_str());
        for part in parts {
            hasher.update_str(part);
        }
        Self::new(kind, format!("@{}", hasher.finish().short()))
    }

    pub fn kind(&self) -> ResourceKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the name was derived rather than written by the operator.
    pub fn is_derived(&self) -> bool {
        self.name.starts_with('@')
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.kind.as_str());
        hasher.update_str(&self.name);
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind, self.name)
    }
}

/// Identifies one process lifetime, so a generation number from a previous
/// process can never be mistaken for a current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId(Fingerprint);

impl BootId {
    pub fn generate() -> Self {
        let mut hasher = Hasher::new();
        hasher.update_u64(std::process::id() as u64);
        hasher.update_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or_default(),
        );
        hasher.update_u64(std::time::Instant::now().elapsed().as_nanos() as u64);
        Self(hasher.finish())
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.short())
    }
}

/// A committed runtime generation. Monotonic within one [`BootId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    pub const NONE: Self = Self(0);
    pub const FIRST: Self = Self(1);

    pub fn number(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_gate_ownership() {
        let file = SourceId::file("main");
        let admin = SourceId::admin();
        assert!(file.owns("api"));
        assert!(!file.owns("runtime/api"));
        assert!(admin.owns("runtime/api"));
        assert!(!admin.owns("api"));
        assert!(!admin.owns("k8s/default/api"));
        assert!(SourceId::kubernetes("default").owns("k8s/default/api"));
    }

    #[test]
    fn derived_ids_depend_on_content_not_order() {
        let a = ResourceId::derived(ResourceKind::Route, &["example.com", "GET", "/api"]);
        let b = ResourceId::derived(ResourceKind::Route, &["example.com", "GET", "/api"]);
        let c = ResourceId::derived(ResourceKind::Route, &["example.com", "GET", "/other"]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.is_derived());
        assert!(!ResourceId::new(ResourceKind::Route, "api").is_derived());
    }

    #[test]
    fn kinds_do_not_collide_in_derived_names() {
        let route = ResourceId::derived(ResourceKind::Route, &["x"]);
        let site = ResourceId::derived(ResourceKind::Site, &["x"]);
        assert_ne!(route.name(), site.name());
    }

    #[test]
    fn generations_start_at_one_and_advance() {
        assert_eq!(Generation::NONE.next(), Some(Generation::FIRST));
        assert_eq!(Generation::FIRST.number(), 1);
    }

    #[test]
    fn boot_ids_differ_between_calls() {
        assert_ne!(BootId::generate().to_string(), BootId::generate().to_string());
    }
}
