//! Getting certificates, and keeping them.
//!
//! Ordering happens in the background, never during prepare. An order takes
//! seconds when everything works and minutes when it does not, and a reload
//! that waited for one would be a reload that a certificate authority's bad
//! afternoon could stall. So a commit publishes the intent — these sites want
//! certificates — and this reconciles against it afterwards, the way health
//! checks do.
//!
//! What arrives goes into the same [`CertificateStore`] as a certificate read
//! from a file. A renewal replaces the entry while established connections
//! carry on and no generation is produced, because a renewal is not something
//! the operator asked for.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use certon::CertStore;
use gatel_model::ResourceId;
use gatel_observe::Telemetry;
use gatel_plan::Acme as AcmeConfig;
use tokio::sync::{Mutex, watch};

use crate::acme::challenge::Challenges;
use crate::acme::issuer::Issuer;
use crate::certificate::Certificate;
use crate::store::CertificateStore;

/// How often the certificates are looked at.
///
/// Renewal windows are measured in days, so this only has to be small enough
/// that a restart does not delay one noticeably.
const CHECK_EVERY: Duration = Duration::from_secs(600);

/// How long to wait before trying again after the authority refused.
///
/// Certificate authorities rate-limit, and hammering one is how an account gets
/// locked out of issuing for a week.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// One site that wants a certificate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wanted {
    pub site: ResourceId,
    /// The single host to certify. A wildcard site cannot get here; normalize
    /// refuses it.
    pub host: String,
}

/// The process's automatic certificates.
pub struct Acme {
    renewal: Arc<Renewal>,
    worker: Mutex<Option<Worker>>,
}

/// Data-plane state has no handle back to the task's lifetime owner.
struct Renewal {
    config: AcmeConfig,
    issuer: Issuer,
    store: Arc<CertificateStore>,
    telemetry: Arc<Telemetry>,
    renew_before: Duration,
    /// When each name may next be ordered for, so a refusal is not retried in a
    /// tight loop.
    next_attempt: Mutex<BTreeMap<String, SystemTime>>,
}

struct Worker {
    wanted: Vec<Wanted>,
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl std::fmt::Debug for Acme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acme")
            .field("issuer", &self.renewal.issuer)
            .finish()
    }
}

impl Acme {
    pub fn new(
        config: &AcmeConfig,
        root: &Path,
        challenges: Arc<Challenges>,
        store: Arc<CertificateStore>,
        telemetry: Arc<Telemetry>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            renewal: Arc::new(Renewal {
                config: config.clone(),
                issuer: Issuer::new(config, root, challenges)?,
                store,
                telemetry,
                renew_before: config.renew_before,
                next_attempt: Mutex::new(BTreeMap::new()),
            }),
            worker: Mutex::new(None),
        }))
    }

    /// Put whatever is already on disk into the store.
    ///
    /// Called during prepare, so a restart serves the certificate it had rather
    /// than waiting for the authority to say it again. Returns whether one was
    /// found; a missing or unreadable file is not an error, because the whole
    /// point is that one will be ordered.
    pub fn adopt(&self, site: &ResourceId, host: &str) -> bool {
        self.renewal.adopt(site, host)
    }

    /// Read persisted material without publishing it to active handshakes.
    pub fn cached(&self, host: &str) -> Option<Certificate> {
        self.renewal.cached(host)
    }

    /// Run a sweep directly. Explicit callers own this future's lifetime.
    pub async fn sweep(&self, wanted: &[Wanted]) {
        self.renewal.sweep(wanted).await;
    }
}

impl Renewal {
    fn adopt(&self, site: &ResourceId, host: &str) -> bool {
        let Some(issuance) = self
            .store
            .issuance(site, crate::store::acme_owner(&self.config, host))
        else {
            return false;
        };
        let Some(certificate) = self.cached(host) else {
            return false;
        };
        if !self.store.put_issued(&issuance, certificate) {
            return false;
        }
        tracing::info!(
            target: "gatel::certificate",
            event = "certificate_loaded",
            site = host,
            source = "acme",
        );
        true
    }

    /// Read persisted material without publishing it to active handshakes.
    pub fn cached(&self, host: &str) -> Option<Certificate> {
        let resource = self.issuer.certificates.load_cached(host).ok()??;
        Certificate::from_pem(
            &resource.certificate_pem,
            &resource.private_key_pem,
            "acme resource",
        )
        .ok()
    }
}

impl Acme {
    /// Order for these sites from now on, and stop caring about any others.
    ///
    /// Called after a commit, never before: a candidate that fails to prepare
    /// must not have asked a certificate authority for anything.
    pub async fn reconcile(self: &Arc<Self>, wanted: Vec<Wanted>) {
        let mut worker = self.worker.lock().await;
        if worker
            .as_ref()
            .is_some_and(|active| active.wanted == wanted && !active.handle.is_finished())
        {
            return;
        }
        if let Some(previous) = worker.as_mut() {
            stop(previous).await;
        }
        worker.take();
        self.renewal.issuer.drain_storage().await;

        let live: BTreeSet<String> = wanted.iter().map(|one| one.host.clone()).collect();
        self.renewal.telemetry.health.retain_certificates(&live);
        if wanted.is_empty() {
            return;
        }
        for one in &wanted {
            if self
                .renewal
                .store
                .issuance(
                    &one.site,
                    crate::store::acme_owner(&self.renewal.config, &one.host),
                )
                .is_some()
            {
                self.renewal
                    .telemetry
                    .health
                    .certificate(&one.host, self.renewal.store.get(&one.site).is_some());
            }
        }

        // Once immediately, so a first boot does not wait out an interval
        // before asking for the certificate it has none of.
        let (stop_sender, mut stopped) = watch::channel(false);
        let owner = Arc::clone(&self.renewal);
        let worker_wanted = wanted.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return,
                    _ = owner.sweep(&wanted) => {},
                }
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return,
                    _ = tokio::time::sleep(CHECK_EVERY) => {},
                }
            }
        });
        *worker = Some(Worker {
            wanted: worker_wanted,
            stop: stop_sender,
            handle,
        });
    }

    pub async fn shutdown(&self) {
        let mut worker = self.worker.lock().await;
        if let Some(worker) = worker.as_mut() {
            stop(worker).await;
        }
        worker.take();
        self.renewal.issuer.drain_storage().await;
    }

    pub async fn is_running(&self) -> bool {
        self.worker
            .lock()
            .await
            .as_ref()
            .is_some_and(|worker| !worker.handle.is_finished())
    }
}

impl Renewal {
    /// Look at every site once: order what is missing, renew what is close.
    pub async fn sweep(&self, wanted: &[Wanted]) {
        for one in wanted {
            if self
                .store
                .issuance(&one.site, crate::store::acme_owner(&self.config, &one.host))
                .is_none()
            {
                continue;
            }
            let ready = self.store.get(&one.site).is_some();
            self.telemetry.health.certificate(&one.host, ready);
            if !self.due(&one.site, &one.host).await {
                continue;
            }
            match self.obtain(one).await {
                Ok(()) => {
                    self.telemetry.health.certificate(&one.host, true);
                }
                Err(error) => {
                    // Never fatal. The site keeps whatever it has, which for a
                    // renewal is a certificate that still works.
                    tracing::warn!(
                        target: "gatel::certificate",
                        event = "certificate_order_failed",
                        site = %one.host,
                        detail = %error,
                    );
                    self.next_attempt
                        .lock()
                        .await
                        .insert(one.host.clone(), SystemTime::now() + RETRY_AFTER);
                }
            }
        }
    }

    /// Whether this name should be ordered for right now.
    async fn due(&self, site: &ResourceId, host: &str) -> bool {
        if let Some(next) = self.next_attempt.lock().await.get(host)
            && SystemTime::now() < *next
        {
            return false;
        }
        match self.store.get(site) {
            // Nothing at all: order.
            None => true,
            // Something: renew only once it is close to the end.
            Some(_) => match self.not_after(host) {
                Some(not_after) => not_after.saturating_sub(now()) <= self.renew_before.as_secs() as i64,
                // On disk but unreadable, which a renewal fixes.
                None => true,
            },
        }
    }

    async fn obtain(&self, wanted: &Wanted) -> Result<(), String> {
        let identity = crate::store::acme_owner(&self.config, &wanted.host);
        let Some(issuance) = self.store.issuance(&wanted.site, identity) else {
            return Err("certificate ownership changed before issuance".into());
        };
        let path = self.issuer.identity_path(&wanted.host);
        let parent = std::fs::canonicalize(self.issuer.directory())
            .map_err(|error| format!("cannot resolve certificate cache directory: {error}"))?;
        let _order = self
            .store
            .orders
            .acquire(parent.join(path.file_name().ok_or("identity path has no filename")?))
            .await;
        let account = super::account_lock::acquire(&parent).await?;
        if !self.store.issuance_is_current(&issuance) {
            return Err("certificate ownership changed while waiting to issue".into());
        }
        // Another order may have completed while this one waited for its turn.
        // Its process has a different CertificateStore; disk is the handoff.
        if self
            .not_after(&wanted.host)
            .is_some_and(|expiry| expiry.saturating_sub(now()) > self.renew_before.as_secs() as i64)
            && let Some(certificate) = self.cached(&wanted.host)
        {
            if !self.store.put_issued(&issuance, certificate) {
                return Err("certificate ownership changed while adopting shared cache".into());
            }
            self.next_attempt.lock().await.remove(&wanted.host);
            return Ok(());
        }
        if !self.due(&wanted.site, &wanted.host).await {
            return Ok(());
        }
        let issued = self.issuer.issue_locked(&wanted.host, account.clone()).await?;
        self.persist_issued(wanted, &issuance, &issued).await?;
        if !self.store.put_issued(&issuance, issued.certificate) {
            return Err("certificate ownership changed during issuance".into());
        }
        self.next_attempt.lock().await.remove(&wanted.host);
        tracing::info!(
            target: "gatel::certificate",
            event = "certificate_issued",
            site = %wanted.host,
            source = "acme",
        );
        Ok(())
    }

    async fn persist_issued(
        &self,
        wanted: &Wanted,
        issuance: &crate::store::Issuance,
        issued: &crate::acme::Issued,
    ) -> Result<(), String> {
        if !self.store.issuance_is_current(issuance) {
            return Err("certificate ownership changed before persistence".into());
        }
        // Written before it is served, so a restart a second later still has
        // it. A failure to write is worth knowing about but not worth
        // discarding a certificate over.
        let resource = certon::storage::CertificateResource {
            sans: vec![wanted.host.clone()],
            certificate_pem: issued.chain_pem.clone(),
            private_key_pem: issued.key_pem.clone(),
            issuer_data: issued.issuer_data.clone(),
            issuer_key: self.issuer.certificates.issuer_key().into(),
        };
        if let Err(error) = self
            .issuer
            .certificates
            .save(self.issuer.certificates.issuer_key(), &resource)
            .await
        {
            tracing::warn!(
                target: "gatel::certificate",
                event = "certificate_not_saved",
                site = %wanted.host,
                detail = %error,
            );
        }
        Ok(())
    }

    /// When the stored certificate for a name expires, in unix seconds.
    fn not_after(&self, host: &str) -> Option<i64> {
        let resource = self.issuer.certificates.load_cached(host).ok()??;
        certon::Certificate::from_pem(&resource.certificate_pem, &resource.private_key_pem)
            .ok()
            .map(|certificate| certificate.not_after.timestamp())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

async fn stop(worker: &mut Worker) {
    let _ = worker.stop.send(true);
    worker.handle.abort();
    let _ = (&mut worker.handle).await;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gatel_model::ResourceKind;

    use super::*;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gatel-acme-manager-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn config() -> AcmeConfig {
        AcmeConfig {
            // Nothing is listening there, which is the point: everything below
            // must work without reaching a certificate authority.
            directory: "http://127.0.0.1:1/directory".to_string(),
            email: "ops@example.com".to_string(),
            agreed: true,
            directory_path: None,
            renew_before: Duration::from_secs(30 * 86_400),
        }
    }

    fn manager(root: &Path) -> Arc<Acme> {
        Acme::new(
            &config(),
            root,
            Challenges::new(),
            Arc::new(CertificateStore::new()),
            Telemetry::new(),
        )
        .unwrap()
    }

    fn wanted(host: &str) -> Wanted {
        Wanted {
            site: ResourceId::new(ResourceKind::Site, host),
            host: host.to_string(),
        }
    }

    #[tokio::test]
    async fn dropping_the_last_manager_cancels_idle_and_blocked_workers() {
        for blocked in [false, true] {
            let root = directory(if blocked {
                "drop-blocked-worker"
            } else {
                "drop-idle-worker"
            });
            let acme = manager(&root);
            let renewal = acme.renewal.clone();
            let gate = if blocked {
                Some(renewal.next_attempt.lock().await)
            } else {
                renewal
                    .next_attempt
                    .lock()
                    .await
                    .insert("example.com".into(), SystemTime::now() + CHECK_EVERY);
                None
            };
            acme.reconcile(vec![wanted("example.com")]).await;
            tokio::task::yield_now().await;
            let manager_weak = Arc::downgrade(&acme);
            let renewal_weak = Arc::downgrade(&renewal);
            drop(acme); // Deliberately omit shutdown, as an embedding host might.
            assert!(
                manager_weak.upgrade().is_none(),
                "worker must not retain its owner"
            );
            drop(gate);
            drop(renewal);
            tokio::time::timeout(Duration::from_secs(1), async {
                while renewal_weak.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("aborted worker releases renewal state without an interval wait");
        }
    }

    #[tokio::test]
    async fn cancelled_shutdown_and_reconcile_do_not_forget_in_flight_storage() {
        let root = directory("storage-shutdown");
        let acme = manager(&root);
        let tasks = acme.renewal.issuer.storage_tasks.clone();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let producer = tokio::spawn(async move {
            tasks
                .run(async move {
                    let _ = started.send(());
                    let _ = released.await;
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        producer.abort();
        assert!(producer.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), acme.shutdown())
                .await
                .is_err()
        );
        // No worker remains, but a retry or replacement must still drain writes.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), acme.reconcile(Vec::new()))
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), acme.shutdown())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), acme.reconcile(Vec::new()))
            .await
            .unwrap();
        assert!(!acme.is_running().await);
    }

    #[tokio::test]
    async fn waiting_manager_adopts_another_managers_fresh_identity_without_an_order() {
        let root = directory("shared-account-cache");
        let first = manager(&root);
        let second = manager(&root);
        let wanted = wanted("example.com");
        let gate = super::super::account_lock::acquire(first.renewal.issuer.directory())
            .await
            .unwrap();
        let attempt = second.renewal.obtain(&wanted);
        tokio::pin!(attempt);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut attempt)
                .await
                .is_err()
        );
        let generated = rcgen::generate_simple_self_signed(vec![wanted.host.clone()]).unwrap();
        super::super::identity_file::write(
            &first.renewal.issuer.identity_path(&wanted.host),
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        assert!(second.renewal.store.is_empty());
        drop(gate);
        // The fixture CA is unreachable: success proves cache reuse.
        tokio::time::timeout(Duration::from_secs(2), attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second.renewal.store.get(&wanted.site).unwrap().certified.cert,
            first.cached(&wanted.host).unwrap().certified.cert
        );
    }

    #[tokio::test]
    async fn account_lock_wait_rechecks_ownership_before_cache_adoption() {
        let root = directory("account-lock-owner");
        let acme = manager(&root);
        let wanted = wanted("example.com");
        let updates = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
        updates.claim(
            wanted.site.clone(),
            crate::store::acme_owner(&acme.renewal.config, &wanted.host),
        );
        updates.begin().publish();
        let gate = super::super::account_lock::acquire(acme.renewal.issuer.directory())
            .await
            .unwrap();
        let attempt = acme.renewal.obtain(&wanted);
        tokio::pin!(attempt);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut attempt)
                .await
                .is_err()
        );
        let mut removed = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
        removed.retain_sites([]);
        removed.begin().publish();
        drop(gate);
        let error = tokio::time::timeout(Duration::from_secs(2), attempt)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("while waiting to issue"), "{error}");
        assert!(acme.renewal.store.is_empty());
    }

    #[tokio::test]
    async fn shared_base_directory_never_adopts_another_account_or_unscoped_identity() {
        let root = directory("cache-namespace");
        let first_config = config();
        let open = |config: &AcmeConfig| {
            Acme::new(
                config,
                &root,
                Challenges::new(),
                Arc::new(CertificateStore::new()),
                Telemetry::new(),
            )
            .unwrap()
        };
        let first = open(&first_config);
        let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let chain = generated.cert.pem();
        let key = generated.signing_key.serialize_pem();
        // Old unscoped records have no trustworthy account provenance.
        super::super::identity_file::write(
            &root.join("acme").join("example.com.identity"),
            chain.as_bytes(),
            key.as_bytes(),
        )
        .unwrap();
        assert!(first.cached("example.com").is_none());
        super::super::identity_file::write(
            &first.renewal.issuer.identity_path("example.com"),
            chain.as_bytes(),
            key.as_bytes(),
        )
        .unwrap();
        assert!(first.cached("example.com").is_some());
        let mut changed = first_config.clone();
        changed.email = "other@example.com".into();
        assert!(open(&changed).cached("example.com").is_none());
        changed = first_config.clone();
        changed.directory = "http://127.0.0.1:2/directory".into();
        assert!(open(&changed).cached("example.com").is_none());
        changed = first_config;
        changed.renew_before *= 2;
        assert_eq!(
            open(&changed).cached("example.com").unwrap().certified.cert,
            first.cached("example.com").unwrap().certified.cert
        );
    }

    #[tokio::test]
    async fn a_queued_order_rechecks_ownership_before_contacting_the_ca() {
        let root = directory("queued-owner");
        let acme = manager(&root);
        let wanted = wanted("example.com");
        let identity = crate::store::acme_owner(&acme.renewal.config, &wanted.host);
        let claim = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
        claim.claim(wanted.site.clone(), identity);
        claim.begin().publish();
        let path = std::fs::canonicalize(acme.renewal.issuer.directory())
            .unwrap()
            .join(
                acme.renewal
                    .issuer
                    .identity_path(&wanted.host)
                    .file_name()
                    .unwrap(),
            );
        let gate = acme.renewal.store.orders.acquire(path).await;
        let attempt = acme.renewal.obtain(&wanted);
        tokio::pin!(attempt);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut attempt)
                .await
                .is_err()
        );
        let mut removed = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
        removed.retain_sites([]);
        removed.begin().publish();
        drop(gate);
        let error = tokio::time::timeout(Duration::from_secs(1), attempt)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("while waiting to issue"), "{error}");
        assert!(!acme.renewal.issuer.identity_path(&wanted.host).exists());
    }

    #[tokio::test]
    async fn stale_order_cannot_overwrite_a_newly_persisted_identity() {
        fn issued() -> crate::acme::Issued {
            let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
            let chain_pem = generated.cert.pem().into_bytes();
            let key_pem = generated.signing_key.serialize_pem().into_bytes();
            crate::acme::Issued {
                certificate: Certificate::from_pem(&chain_pem, &key_pem, "fixture").unwrap(),
                chain_pem,
                key_pem,
                not_after: now() + 3600,
                issuer_data: None,
            }
        }
        let root = directory("stale-persist");
        let acme = manager(&root);
        let wanted = wanted("example.com");
        let identity = crate::store::acme_owner(&acme.renewal.config, &wanted.host);
        let publish = || {
            let updates = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
            updates.claim(wanted.site.clone(), identity);
            updates.begin().publish();
        };
        publish();
        let stale = acme.renewal.store.issuance(&wanted.site, identity).unwrap();
        let mut removed = crate::store::CertificateUpdates::new(acme.renewal.store.clone());
        removed.retain_sites([]);
        removed.begin().publish();
        publish();
        let fresh = acme.renewal.store.issuance(&wanted.site, identity).unwrap();
        let current = issued();
        acme.renewal
            .persist_issued(&wanted, &fresh, &current)
            .await
            .unwrap();
        assert!(
            acme.renewal
                .persist_issued(&wanted, &stale, &issued())
                .await
                .unwrap_err()
                .contains("before persistence")
        );
        assert_eq!(
            acme.cached(&wanted.host).unwrap().certified.cert,
            current.certificate.certified.cert
        );
    }

    #[tokio::test]
    async fn corrupt_new_identity_does_not_fall_back_to_a_stale_legacy_pair() {
        let root = directory("identity-fallback");
        let acme = manager(&root);
        let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let (chain, key) = acme.renewal.issuer.paths("example.com");
        std::fs::write(chain, generated.cert.pem()).unwrap();
        crate::certificate::write_key(&key, &generated.signing_key.serialize_pem());
        assert!(acme.cached("example.com").is_some());
        assert!(acme.renewal.not_after("example.com").is_some());
        crate::certificate::write_key(
            &acme.renewal.issuer.identity_path("example.com"),
            "broken identity",
        );
        assert!(acme.cached("example.com").is_none());
        assert!(acme.renewal.not_after("example.com").is_none());
    }

    #[tokio::test]
    async fn activation_does_not_wait_for_a_sweep_and_shutdown_cancels_it() {
        let root = directory("cancel-sweep");
        let acme = manager(&root);
        let gate = acme.renewal.next_attempt.lock().await;
        let sites = vec![wanted("example.com")];
        tokio::time::timeout(Duration::from_secs(1), acme.reconcile(sites.clone()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let worker_id = acme.worker.lock().await.as_ref().unwrap().handle.id();
        acme.reconcile(sites).await;
        assert_eq!(
            acme.worker.lock().await.as_ref().unwrap().handle.id(),
            worker_id,
            "compatible reload keeps in-flight work"
        );
        tokio::time::timeout(Duration::from_secs(1), acme.shutdown())
            .await
            .unwrap();
        assert!(!acme.is_running().await);
        assert_eq!(
            Arc::strong_count(&acme),
            1,
            "worker has been joined, not detached"
        );
        drop(gate);
    }

    #[tokio::test]
    async fn a_site_with_no_certificate_is_reported_as_pending() {
        let root = directory("pending");
        let acme = manager(&root);
        acme.sweep(&[wanted("example.com")]).await;

        // Degraded, not down: this instance is still serving everything else,
        // and every replica would say the same thing at the same moment.
        let report = acme.renewal.telemetry.health.report();
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.code() == "certificate_pending"),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn a_refusal_is_not_retried_immediately() {
        // A certificate authority that says no is rate-limiting, and hammering
        // one is how an account loses issuing for a week.
        let root = directory("backoff");
        let acme = manager(&root);
        let sites = [wanted("example.com")];
        acme.sweep(&sites).await;
        assert!(
            !acme.renewal.due(&sites[0].site, "example.com").await,
            "a second attempt must wait"
        );
    }

    #[tokio::test]
    async fn a_name_nothing_was_ordered_for_is_due() {
        let root = directory("due");
        let acme = manager(&root);
        assert!(
            acme.renewal
                .due(&ResourceId::new(ResourceKind::Site, "a"), "a")
                .await
        );
    }

    #[tokio::test]
    async fn nothing_on_disk_is_nothing_to_adopt() {
        let root = directory("adopt-missing");
        let acme = manager(&root);
        assert!(!acme.adopt(&ResourceId::new(ResourceKind::Site, "a"), "a.example.com"));
        assert!(acme.renewal.store.is_empty());
    }

    #[tokio::test]
    async fn reconciling_with_nothing_starts_no_worker() {
        let root = directory("empty");
        let acme = manager(&root);
        acme.reconcile(Vec::new()).await;
        assert!(!acme.is_running().await);
    }

    #[tokio::test]
    async fn reconciling_starts_one_worker_and_shutdown_stops_it() {
        let root = directory("worker");
        let acme = manager(&root);
        acme.reconcile(vec![wanted("example.com")]).await;
        assert!(acme.is_running().await);
        acme.shutdown().await;
        assert!(!acme.is_running().await);
    }

    #[tokio::test]
    async fn a_site_a_reload_dropped_stops_being_reported() {
        let root = directory("retain");
        let acme = manager(&root);
        acme.reconcile(vec![wanted("old.example.com")]).await;
        assert!(
            acme.renewal
                .telemetry
                .health
                .report()
                .reasons
                .iter()
                .any(|reason| reason.code() == "certificate_pending")
        );

        acme.reconcile(Vec::new()).await;
        assert!(
            !acme
                .renewal
                .telemetry
                .health
                .report()
                .reasons
                .iter()
                .any(|reason| reason.code() == "certificate_pending"),
            "a site that is gone is not still waiting for anything"
        );
        acme.shutdown().await;
    }
}
