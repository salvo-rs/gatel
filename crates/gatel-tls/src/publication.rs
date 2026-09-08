//! Candidate certificate deltas; never snapshot and overwrite concurrent renewals.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLockWriteGuard};

use gatel_model::ResourceId;

use super::{Certificate, CertificateStore, Ownership};

struct Update {
    certificate: Option<Certificate>,
    only_if_missing: bool,
    owner: Option<gatel_model::Fingerprint>,
}

pub struct CertificateUpdates {
    published: std::sync::atomic::AtomicBool,
    live: Option<std::collections::BTreeSet<ResourceId>>,
    store: Arc<CertificateStore>,
    pending: Mutex<BTreeMap<ResourceId, Update>>,
    retired: Mutex<Vec<Certificate>>,
}

impl std::fmt::Debug for CertificateUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateUpdates").finish_non_exhaustive()
    }
}

impl CertificateUpdates {
    /// Release superseded material after all host publication locks are gone.
    /// Readers that still own a certified key retain their independent Arc.
    pub fn finish(&self) {
        let retired = std::mem::take(&mut *self.retired.lock().unwrap_or_else(|p| p.into_inner()));
        drop(retired);
    }

    /// The HTTP host publishes a complete set of TLS owners, including removals.
    pub fn retain_sites(&mut self, sites: impl IntoIterator<Item = ResourceId>) {
        self.live = Some(sites.into_iter().collect());
    }
    /// Declare provenance even when ACME has no certificate yet.
    pub fn claim(&self, site: ResourceId, identity: gatel_model::Fingerprint) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(site)
            .or_insert(Update {
                certificate: None,
                only_if_missing: true,
                owner: Some(identity),
            });
    }
    pub fn new(store: Arc<CertificateStore>) -> Self {
        Self {
            published: std::sync::atomic::AtomicBool::new(false),
            live: None,
            store,
            pending: Mutex::new(BTreeMap::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    pub fn get(&self, site: &ResourceId) -> Option<Certificate> {
        let pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        self.get_pending(site, &pending)
    }

    fn get_pending(&self, site: &ResourceId, pending: &BTreeMap<ResourceId, Update>) -> Option<Certificate> {
        let update = pending.get(site);
        if let Some(certificate) = update.and_then(|entry| entry.certificate.clone()) {
            return Some(certificate);
        }
        let owners = self.store.owners.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(identity) = update.and_then(|entry| entry.owner)
            && owners.get(site).is_none_or(|owner| owner.identity != identity)
        {
            return None;
        }
        self.store.get(site)
    }

    /// Manual replacement is explicit. Internal/cached certificates only fill
    /// an empty slot, rechecked at publication so a concurrent renewal wins.
    pub fn put(&self, site: ResourceId, certificate: Certificate, only_if_missing: bool) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let existing = self.get_pending(&site, &pending);
        let owner = pending.get(&site).and_then(|entry| entry.owner);
        if only_if_missing && existing.is_some() {
            return false;
        }
        let changed = existing.is_none_or(|old| old.certified.cert != certificate.certified.cert);
        pending.insert(
            site,
            Update {
                certificate: Some(certificate),
                only_if_missing,
                owner,
            },
        );
        changed
    }

    /// Host calls this after generation/source stale checks. No I/O here.
    pub fn begin(&self) -> CertificatePublication<'_> {
        CertificatePublication {
            published: &self.published,
            live: self.live.as_ref(),
            managed: &self.store.managed,
            pending: self.pending.lock().unwrap_or_else(|p| p.into_inner()),
            owners: self.store.owners.lock().unwrap_or_else(|p| p.into_inner()),
            entries: self.store.entries.write().unwrap_or_else(|p| p.into_inner()),
            retired: self.retired.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }
}

pub struct CertificatePublication<'a> {
    published: &'a std::sync::atomic::AtomicBool,
    live: Option<&'a std::collections::BTreeSet<ResourceId>>,
    managed: &'a std::sync::atomic::AtomicBool,
    owners: MutexGuard<'a, BTreeMap<ResourceId, Ownership>>,
    pending: MutexGuard<'a, BTreeMap<ResourceId, Update>>,
    entries: RwLockWriteGuard<'a, BTreeMap<ResourceId, Certificate>>,
    retired: MutexGuard<'a, Vec<Certificate>>,
}

impl CertificatePublication<'_> {
    pub fn publish(mut self) {
        if self.published.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Some(live) = self.live {
            self.managed.store(true, std::sync::atomic::Ordering::Relaxed);
            self.owners.retain(|site, _| live.contains(site));
            let removed: Vec<_> = self
                .entries
                .keys()
                .filter(|site| !live.contains(site))
                .cloned()
                .collect();
            for site in removed {
                if let Some(old) = self.entries.remove(&site) {
                    self.retired.push(old);
                }
            }
        }
        for (site, update) in std::mem::take(&mut *self.pending) {
            if let Some(identity) = update.owner
                && self
                    .owners
                    .get(&site)
                    .is_none_or(|owner| owner.identity != identity)
            {
                self.owners.insert(
                    site.clone(),
                    Ownership {
                        identity,
                        token: Arc::new(()),
                    },
                );
                if let Some(old) = self.entries.remove(&site) {
                    self.retired.push(old);
                }
            }
            let Some(certificate) = update.certificate else {
                continue;
            };
            if update.only_if_missing && self.entries.contains_key(&site) {
                self.retired.push(certificate);
            } else if let Some(old) = self.entries.insert(site, certificate) {
                self.retired.push(old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn certificate() -> Certificate {
        let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        Certificate::from_pem(
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
            "fixture",
        )
        .unwrap()
    }
    fn site(name: &str) -> ResourceId {
        ResourceId::new(gatel_model::ResourceKind::Site, name)
    }

    fn identity(name: &str) -> gatel_model::Fingerprint {
        let mut hash = gatel_model::Hasher::new();
        hash.update_str(name);
        hash.finish()
    }

    #[test]
    fn source_changes_revoke_old_issuance_even_after_switching_back() {
        let store = Arc::new(CertificateStore::new());
        let id = site("one");
        let publish = |owner| {
            let mut updates = CertificateUpdates::new(store.clone());
            updates.claim(id.clone(), owner);
            updates.retain_sites([id.clone()]);
            updates.begin().publish();
        };
        publish(identity("acme"));
        let old = store.issuance(&id, identity("acme")).unwrap();
        assert!(store.put_issued(&old, certificate()));
        // Compatible claims preserve both certificate and authority to renew.
        publish(identity("acme"));
        assert!(store.put_issued(&old, certificate()));
        publish(identity("manual"));
        assert!(store.get(&id).is_none());
        assert!(store.issuance(&id, identity("acme")).is_none());
        assert!(!store.put_issued(&old, certificate()));
        publish(identity("acme"));
        assert!(
            !store.put_issued(&old, certificate()),
            "ABA must not revive an old order"
        );
        let current = store.issuance(&id, identity("acme")).unwrap();
        assert!(store.put_issued(&current, certificate()));
        let mut removal = CertificateUpdates::new(store.clone());
        removal.retain_sites([]);
        removal.begin().publish();
        assert!(store.is_empty());
        assert!(!store.put_issued(&current, certificate()));
        assert!(store.issuance(&id, identity("acme")).is_none());
        publish(identity("acme"));
        assert!(store.issuance(&id, identity("acme")).is_some());
        removal.begin().publish();
        assert!(
            store.issuance(&id, identity("acme")).is_some(),
            "replayed removal is inert"
        );
    }

    #[test]
    fn an_uncommitted_claim_cannot_revoke_the_active_issuer() {
        let store = Arc::new(CertificateStore::new());
        let id = site("one");
        let active = CertificateUpdates::new(store.clone());
        active.claim(id.clone(), identity("acme"));
        active.begin().publish();
        let permit = store.issuance(&id, identity("acme")).unwrap();
        let candidate = CertificateUpdates::new(store.clone());
        candidate.claim(id.clone(), identity("manual"));
        drop(candidate.begin());
        drop(candidate);
        assert!(store.put_issued(&permit, certificate()));
    }

    #[test]
    fn discarded_updates_and_guards_leave_active_certificates_unchanged() {
        let store = Arc::new(CertificateStore::new());
        let id = site("one");
        let original = certificate();
        store.put(id.clone(), original.clone());
        let updates = CertificateUpdates::new(store.clone());
        updates.put(id.clone(), certificate(), false);
        updates.put(site("new"), certificate(), true);
        drop(updates.begin());
        assert_eq!(store.get(&id).unwrap().certified.cert, original.certified.cert);
        assert_eq!(store.len(), 1);
        drop(updates);
        assert_eq!(store.get(&id).unwrap().certified.cert, original.certified.cert);
    }

    #[test]
    fn finish_releases_replaced_removed_and_unused_material_without_revoking_readers() {
        let store = Arc::new(CertificateStore::new());
        let original = certificate();
        let reader = original.certified.clone();
        let old = Arc::downgrade(&original.certified);
        store.put(site("replace"), original);
        let removed = certificate();
        let removed_key = Arc::downgrade(&removed.certified);
        store.put(site("remove"), removed);
        let unused = certificate();
        let unused_key = Arc::downgrade(&unused.certified);
        let mut updates = CertificateUpdates::new(store.clone());
        updates.retain_sites([site("replace"), site("cached")]);
        updates.put(site("replace"), certificate(), false);
        // Race a renewal into the cache slot after preparing the candidate.
        updates.put(site("cached"), unused, true);
        store.put(site("cached"), certificate());
        updates.begin().publish();
        assert!(removed_key.upgrade().is_some());
        assert!(unused_key.upgrade().is_some());
        updates.finish();
        assert_eq!(removed_key.strong_count(), 0);
        assert_eq!(unused_key.strong_count(), 0);
        assert_eq!(old.strong_count(), 1, "existing TLS reader retains its key");
        assert_eq!(store.len(), 2);
        updates.finish();
        drop(reader);
        assert_eq!(old.strong_count(), 0);
    }

    #[test]
    fn finishing_older_publication_cannot_remove_a_newer_certificate() {
        let store = Arc::new(CertificateStore::new());
        let id = site("replace");
        let old = certificate();
        let old_key = Arc::downgrade(&old.certified);
        store.put(id.clone(), old);
        let first = CertificateUpdates::new(store.clone());
        let middle = certificate();
        let middle_key = Arc::downgrade(&middle.certified);
        first.put(id.clone(), middle, false);
        first.begin().publish();
        let second = CertificateUpdates::new(store.clone());
        let newest = certificate();
        second.put(id.clone(), newest.clone(), false);
        second.begin().publish();
        second.finish();
        assert_eq!(middle_key.strong_count(), 0);
        assert_eq!(old_key.strong_count(), 1);
        first.finish();
        assert_eq!(old_key.strong_count(), 0);
        assert!(Arc::ptr_eq(&newest.certified, &store.get(&id).unwrap().certified));
    }

    #[test]
    fn deltas_preserve_concurrent_renewal_and_cached_adoption_never_overwrites_it() {
        let store = Arc::new(CertificateStore::new());
        let updates = CertificateUpdates::new(store.clone());
        updates.put(site("cached"), certificate(), true);
        let manual = certificate();
        updates.put(site("manual"), manual.clone(), false);
        let renewal = certificate();
        store.put(site("cached"), renewal.clone());
        store.put(site("unrelated"), renewal.clone());
        updates.begin().publish();
        assert_eq!(
            store.get(&site("cached")).unwrap().certified.cert,
            renewal.certified.cert
        );
        assert_eq!(
            store.get(&site("unrelated")).unwrap().certified.cert,
            renewal.certified.cert
        );
        assert_eq!(
            store.get(&site("manual")).unwrap().certified.cert,
            manual.certified.cert
        );
        // A repeated publication must not replay the delta over a newer renewal.
        store.put(site("manual"), renewal.clone());
        updates.begin().publish();
        assert_eq!(
            store.get(&site("manual")).unwrap().certified.cert,
            renewal.certified.cert
        );
    }
}
