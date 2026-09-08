//! Host publication boundary. Not a plugin callback or general transaction engine.
use std::sync::Arc;

/// A prepared runtime locks its already-validated shared-state changes here.
/// Called only after stale checks, under the generation write lock. Implementors
/// must not perform I/O, await, reenter the store, or invoke plugin code.
pub trait PublishRuntime {
    type Guard<'a>: PublishGuard
    where
        Self: 'a;
    fn begin_publication(&self) -> Self::Guard<'_>;
    /// Retire replaced ownership after the generation lock and every runtime
    /// publication guard are released. This is not part of the atomic swap;
    /// destructors of plugin-owned state may execute here.
    /// A newer generation may already be current: release only this candidate's
    /// retired ownership, never infer what to delete from the current store.
    fn finish_publication(&self) {}
}

/// Held until the new generation is published. Dropping without `published`
/// must roll back staged changes. `published` must be infallible and non-panicking.
pub trait PublishGuard {
    fn published(self);
}
impl PublishGuard for () {
    fn published(self) {}
}
impl PublishRuntime for () {
    type Guard<'a> = ();
    fn begin_publication(&self) {}
}
impl<T: PublishRuntime> PublishRuntime for Arc<T> {
    type Guard<'a>
        = T::Guard<'a>
    where
        T: 'a;
    fn begin_publication(&self) -> Self::Guard<'_> {
        (**self).begin_publication()
    }
    fn finish_publication(&self) {
        (**self).finish_publication();
    }
}
impl<T: PublishGuard> PublishGuard for Option<T> {
    fn published(self) {
        if let Some(guard) = self {
            guard.published();
        }
    }
}
impl PublishGuard for gatel_plugin_runtime::blocking::Publication<'_> {
    fn published(self) {
        self.publish();
    }
}

#[cfg(test)]
impl PublishRuntime for usize {
    type Guard<'a> = ();
    fn begin_publication(&self) {}
}
