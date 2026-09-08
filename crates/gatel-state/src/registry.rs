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

#[derive(Clone)]
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
#[derive(Clone, Default)]
pub struct StateRegistry {
    slots: Arc<Mutex<BTreeMap<StateKey, Slot>>>,
}

/// Prepared registration changes. Old entries are retained outside the commit
/// guard so plugin-owned values are not destroyed under the generation lock.
pub struct StateUpdate {
    target: StateRegistry,
    pending: Mutex<Option<BTreeMap<StateKey, Slot>>>,
    retired: Mutex<Option<BTreeMap<StateKey, Slot>>>,
}

impl std::fmt::Debug for StateUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateUpdate").finish_non_exhaustive()
    }
}

pub struct StatePublication<'a> {
    slots: std::sync::MutexGuard<'a, BTreeMap<StateKey, Slot>>,
    pending: std::sync::MutexGuard<'a, Option<BTreeMap<StateKey, Slot>>>,
    retired: std::sync::MutexGuard<'a, Option<BTreeMap<StateKey, Slot>>>,
}

impl StateUpdate {
    /// Release publication's previous values after all host publication guards
    /// have been released. Plugin destructors may run here, never under a lock.
    pub fn finish(&self) {
        let retired = self.retired.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(retired);
    }

    pub fn begin(&self) -> StatePublication<'_> {
        StatePublication {
            slots: self.target.slots.lock().unwrap_or_else(|p| p.into_inner()),
            pending: self.pending.lock().unwrap_or_else(|p| p.into_inner()),
            retired: self.retired.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }
}

impl StatePublication<'_> {
    pub fn publish(mut self) {
        if let Some(next) = self.pending.take() {
            *self.retired = Some(std::mem::replace(&mut *self.slots, next));
        }
    }
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
    /// Independent registrations with shared compatible values. Candidate
    /// factories must not mutate reused values during preparation.
    pub fn stage(&self) -> Self {
        Self {
            slots: Arc::new(Mutex::new(
                self.slots.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            )),
        }
    }

    /// Seal the staged registrations for publication after host stale checks.
    /// The host must fence competing generations; this is not its own CAS.
    pub fn update_from(&self, staged: &Self) -> StateUpdate {
        StateUpdate {
            target: self.clone(),
            pending: Mutex::new(Some(
                staged.slots.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            )),
            retired: Mutex::new(None),
        }
    }
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

    /// Resolve a binding whose type is only known to the directive that
    /// declared it.
    ///
    /// The same reuse rule as [`StateRegistry::bind`], for callers that hold an
    /// already-erased value: a directive builds its own state, so the registry
    /// never needs to know what is inside.
    pub fn bind_any(
        &self,
        binding: &StateBinding,
        create: &dyn Fn() -> Arc<dyn Any + Send + Sync>,
    ) -> (Arc<dyn Any + Send + Sync>, Resolution) {
        let key = StateKey {
            owner: binding.owner.clone(),
            kind: binding.kind,
        };
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());

        if binding.policy == ResetPolicy::Preserve
            && let Some(slot) = slots.get(&key)
            && slot.compatibility == binding.compatibility
        {
            return (Arc::clone(&slot.value), Resolution::Reused);
        }

        let replaced = slots.contains_key(&key);
        let value = create();
        slots.insert(
            key,
            Slot {
                compatibility: binding.compatibility,
                value: Arc::clone(&value),
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
    fn staged_replacement_is_invisible_until_publication() {
        let registry = StateRegistry::new();
        let original = binding(StateKind::Pool, "original", ResetPolicy::Preserve);
        let (first, _) = registry.bind(&original, || 1usize);
        let staged = registry.stage();
        let (reused, resolution) = staged.bind(&original, || 99usize);
        assert_eq!(resolution, Resolution::Reused);
        assert!(Arc::ptr_eq(&first, &reused));
        let changed = binding(StateKind::Pool, "changed", ResetPolicy::Preserve);
        let (next, _) = staged.bind(&changed, || 2usize);
        let update = registry.update_from(&staged);
        drop(update.begin());
        assert!(Arc::ptr_eq(
            &first,
            &registry.get::<usize>(&original.owner, original.kind).unwrap()
        ));
        update.begin().publish();
        assert!(Arc::ptr_eq(
            &next,
            &registry.get::<usize>(&original.owner, original.kind).unwrap()
        ));
        // Publication is one-shot, including when accidentally called again.
        update.begin().publish();
        assert_eq!(*registry.get::<usize>(&original.owner, original.kind).unwrap(), 2);
    }

    #[test]
    fn finishing_publication_drops_retired_values_outside_registry_locks() {
        struct Reenter {
            registry: StateRegistry,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Reenter {
            fn drop(&mut self) {
                assert!(
                    self.registry.slots.try_lock().is_ok(),
                    "registry lock held during destructor"
                );
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        let registry = StateRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let (value, _) = registry.bind(&binding(StateKind::Pool, "old", ResetPolicy::Preserve), || {
            Reenter {
                registry: registry.clone(),
                drops: drops.clone(),
            }
        });
        let weak = Arc::downgrade(&value);
        drop(value);
        let update = registry.update_from(&StateRegistry::new());
        update.begin().publish();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "publish only transfers ownership"
        );
        update.finish();
        assert_eq!(weak.strong_count(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        update.finish();
        assert_eq!(drops.load(Ordering::SeqCst), 1, "finish is idempotent");
    }

    #[test]
    fn late_finish_only_releases_its_own_retired_values() {
        let registry = StateRegistry::new();
        let old_binding = binding(StateKind::Pool, "old", ResetPolicy::Preserve);
        let (old, _) = registry.bind(&old_binding, || 1usize);
        let old_weak = Arc::downgrade(&old);
        drop(old);
        let middle = registry.stage();
        let (middle_value, _) = middle
            .bind(&binding(StateKind::Pool, "middle", ResetPolicy::Preserve), || {
                2usize
            });
        let middle_weak = Arc::downgrade(&middle_value);
        drop(middle_value);
        let first = registry.update_from(&middle);
        drop(middle);
        first.begin().publish();
        let newest = registry.stage();
        let (value, _) = newest.bind(&binding(StateKind::Pool, "newest", ResetPolicy::Preserve), || {
            3usize
        });
        let second = registry.update_from(&newest);
        drop(newest);
        second.begin().publish();
        second.finish();
        assert_eq!(middle_weak.strong_count(), 0);
        assert_eq!(old_weak.strong_count(), 1);
        first.finish();
        assert_eq!(old_weak.strong_count(), 0);
        assert!(Arc::ptr_eq(
            &value,
            &registry
                .get::<usize>(&old_binding.owner, old_binding.kind)
                .unwrap()
        ));
    }

    #[test]
    fn discarded_candidate_does_not_replace_registrations() {
        let registry = StateRegistry::new();
        let original = binding(StateKind::Pool, "original", ResetPolicy::Preserve);
        let (first, _) = registry.bind(&original, || 1usize);
        let staged = registry.stage();
        staged.bind(
            &binding(StateKind::Pool, "changed", ResetPolicy::Preserve),
            || 2usize,
        );
        staged.bind(
            &binding(StateKind::Selector, "new", ResetPolicy::Preserve),
            || 3usize,
        );
        drop(registry.update_from(&staged));
        drop(staged);
        assert_eq!(registry.len(), 1);
        assert!(Arc::ptr_eq(
            &first,
            &registry.get::<usize>(&original.owner, original.kind).unwrap()
        ));
    }

    #[test]
    fn publication_does_not_destroy_displaced_plugin_values() {
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let registry = StateRegistry::new();
        let original = binding(StateKind::Pool, "original", ResetPolicy::Preserve);
        registry.bind(&original, || Dropped(drops.clone()));
        let staged = registry.stage();
        staged.bind(
            &binding(StateKind::Pool, "changed", ResetPolicy::Preserve),
            || 2usize,
        );
        let update = registry.update_from(&staged);
        update.begin().publish();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(update);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
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
