//! The runtime state registry.
//!
//! State is keyed by the resource that owns it and the kind of state it is, so
//! a reload can ask "is what I need already running?" and get a typed answer.
//! Nothing here is reachable except through a binding the plan declared, which
//! is what stops a directive from quietly keeping a global singleton.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gatel_model::{Fingerprint, ResourceId};
use gatel_plan::{ResetPolicy, StateBinding, StateKind};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StateKey {
    pub owner: ResourceId,
    pub kind: StateKind,
}

impl std::fmt::Display for StateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.kind.as_str())
    }
}

struct Slot {
    compatibility: Fingerprint,
    value: Arc<dyn Any + Send + Sync>,
}

/// What happened when a binding was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Nothing existed; a new instance was created.
    Created,
    /// The existing instance was compatible and carried over.
    Reused,
    /// The existing instance could not be carried over and was replaced.
    Replaced,
}

/// Typed, owner-scoped mutable state that outlives generations.
#[derive(Default)]
pub struct StateRegistry {
    slots: Mutex<BTreeMap<StateKey, Slot>>,
}

impl std::fmt::Debug for StateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("StateRegistry")
            .field("slots", &slots.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl StateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a binding: reuse the running instance if it is compatible, and
    /// otherwise build a new one.
    ///
    /// The old instance is dropped from the registry but not from the
    /// generations still using it, so it drains rather than disappearing.
    pub fn bind<T, F>(&self, binding: &StateBinding, create: F) -> (Arc<T>, Resolution)
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T,
    {
        let key = StateKey {
            owner: binding.owner.clone(),
            kind: binding.kind,
        };
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());

        if binding.policy == ResetPolicy::Preserve
            && let Some(slot) = slots.get(&key)
            && slot.compatibility == binding.compatibility
            && let Ok(existing) = Arc::clone(&slot.value).downcast::<T>()
        {
            return (existing, Resolution::Reused);
        }

        let replaced = slots.contains_key(&key);
        let value = Arc::new(create());
        slots.insert(
            key,
            Slot {
                compatibility: binding.compatibility,
                value: Arc::clone(&value) as Arc<dyn Any + Send + Sync>,
            },
        );
        (
            value,
            if replaced {
                Resolution::Replaced
            } else {
                Resolution::Created
            },
        )
    }

    /// Look up running state without creating it.
    pub fn get<T: Send + Sync + 'static>(&self, owner: &ResourceId, kind: StateKind) -> Option<Arc<T>> {
        let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        let slot = slots.get(&StateKey {
            owner: owner.clone(),
            kind,
        })?;
        Arc::clone(&slot.value).downcast::<T>().ok()
    }

    /// Forget everything no longer named by a plan. Live generations keep what
    /// they hold, so this releases rather than interrupts.
    pub fn retain(&self, bindings: &[StateBinding]) -> Vec<StateKey> {
        let keep: Vec<StateKey> = bindings
            .iter()
            .map(|binding| StateKey {
                owner: binding.owner.clone(),
                kind: binding.kind,
            })
            .collect();
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        let retired: Vec<StateKey> = slots.keys().filter(|key| !keep.contains(key)).cloned().collect();
        for key in &retired {
            slots.remove(key);
        }
        retired
    }

    pub fn keys(&self) -> Vec<StateKey> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gatel_model::{Hasher, ResourceKind};

    use super::*;

    #[derive(Debug)]
    struct Counter(AtomicUsize);

    fn fingerprint(tag: &str) -> Fingerprint {
        let mut hasher = Hasher::new();
        hasher.update_str(tag);
        hasher.finish()
    }

    fn binding(kind: StateKind, tag: &str, policy: ResetPolicy) -> StateBinding {
        StateBinding {
            owner: ResourceId::new(ResourceKind::Upstream, "api"),
            kind,
            compatibility: fingerprint(tag),
            policy,
        }
    }

    #[test]
    fn a_compatible_binding_carries_state_across_reloads() {
        let registry = StateRegistry::new();
        let binding = binding(StateKind::Pool, "same", ResetPolicy::Preserve);

        let (first, resolution) = registry.bind(&binding, || Counter(AtomicUsize::new(0)));
        assert_eq!(resolution, Resolution::Created);
        first.0.store(42, Ordering::Relaxed);

        let (second, resolution) = registry.bind(&binding, || Counter(AtomicUsize::new(0)));
        assert_eq!(resolution, Resolution::Reused);
        assert_eq!(
            second.0.load(Ordering::Relaxed),
            42,
            "the counter must survive the reload"
        );
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn an_incompatible_binding_replaces_the_instance() {
        let registry = StateRegistry::new();
        let (first, _) = registry.bind(&binding(StateKind::Pool, "before", ResetPolicy::Preserve), || {
            Counter(AtomicUsize::new(7))
        });
        let (second, resolution) = registry
            .bind(&binding(StateKind::Pool, "after", ResetPolicy::Preserve), || {
                Counter(AtomicUsize::new(0))
            });
        assert_eq!(resolution, Resolution::Replaced);
        assert!(!Arc::ptr_eq(&first, &second));
        // The old instance still exists for whoever holds it: it drains.
        assert_eq!(first.0.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn a_reset_policy_always_starts_fresh() {
        let registry = StateRegistry::new();
        let binding = binding(StateKind::Selector, "same", ResetPolicy::Reset);
        let (first, _) = registry.bind(&binding, || Counter(AtomicUsize::new(1)));
        let (second, resolution) = registry.bind(&binding, || Counter(AtomicUsize::new(0)));
        assert_eq!(resolution, Resolution::Replaced);
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn kinds_do_not_share_a_slot() {
        let registry = StateRegistry::new();
        registry.bind(&binding(StateKind::Pool, "x", ResetPolicy::Preserve), || {
            Counter(AtomicUsize::new(1))
        });
        registry.bind(&binding(StateKind::Selector, "x", ResetPolicy::Preserve), || {
            Counter(AtomicUsize::new(2))
        });
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn state_of_the_wrong_type_is_rebuilt_rather_than_mis_cast() {
        let registry = StateRegistry::new();
        let binding = binding(StateKind::Pool, "x", ResetPolicy::Preserve);
        registry.bind(&binding, || Counter(AtomicUsize::new(1)));
        let (value, resolution) = registry.bind::<String, _>(&binding, || "fresh".to_string());
        assert_eq!(resolution, Resolution::Replaced);
        assert_eq!(*value, "fresh");
    }

    #[test]
    fn retaining_drops_only_what_is_no_longer_named() {
        let registry = StateRegistry::new();
        let pool = binding(StateKind::Pool, "x", ResetPolicy::Preserve);
        let selector = binding(StateKind::Selector, "x", ResetPolicy::Preserve);
        registry.bind(&pool, || Counter(AtomicUsize::new(1)));
        registry.bind(&selector, || Counter(AtomicUsize::new(2)));

        let retired = registry.retain(&[pool]);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].kind, StateKind::Selector);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn lookup_finds_running_state_without_creating_it() {
        let registry = StateRegistry::new();
        let owner = ResourceId::new(ResourceKind::Upstream, "api");
        assert!(registry.get::<Counter>(&owner, StateKind::Pool).is_none());
        registry.bind(&binding(StateKind::Pool, "x", ResetPolicy::Preserve), || {
            Counter(AtomicUsize::new(9))
        });
        let found = registry.get::<Counter>(&owner, StateKind::Pool).unwrap();
        assert_eq!(found.0.load(Ordering::Relaxed), 9);
    }
}
