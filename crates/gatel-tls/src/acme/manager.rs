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
    issuer: Issuer,
    store: Arc<CertificateStore>,
    telemetry: Arc<Telemetry>,
    renew_before: Duration,
    /// When each name may next be ordered for, so a refusal is not retried in a
    /// tight loop.
    next_attempt: Mutex<BTreeMap<String, SystemTime>>,
    worker: Mutex<Option<Worker>>,
}

struct Worker {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Acme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acme").field("issuer", &self.issuer).finish()
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
            issuer: Issuer::new(config, root, challenges)?,
            store,
            telemetry,
            renew_before: config.renew_before,
            next_attempt: Mutex::new(BTreeMap::new()),
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
        let (chain, key) = self.issuer.paths(host);
        let Ok(certificate) = Certificate::from_pem_files(&chain, &key) else {
            return false;
        };
        self.store.put(site.clone(), certificate);
        tracing::info!(
            target: "gatel::certificate",
            event = "certificate_loaded",
            site = host,
            source = "acme",
        );
        true
    }

    /// Order for these sites from now on, and stop caring about any others.
    ///
    /// Called after a commit, never before: a candidate that fails to prepare
    /// must not have asked a certificate authority for anything.
    pub async fn reconcile(self: &Arc<Self>, wanted: Vec<Wanted>) {
        let mut worker = self.worker.lock().await;
        if let Some(previous) = worker.take() {
            stop(previous).await;
        }

        let live: BTreeSet<String> = wanted.iter().map(|one| one.host.clone()).collect();
        self.telemetry.health.retain_certificates(&live);
        if wanted.is_empty() {
            return;
        }

        // Once immediately, so a first boot does not wait out an interval
        // before asking for the certificate it has none of.
        self.sweep(&wanted).await;

        let (stop_sender, mut stopped) = watch::channel(false);
        let owner = Arc::clone(self);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => return,
                    _ = tokio::time::sleep(CHECK_EVERY) => owner.sweep(&wanted).await,
                }
            }
        });
        *worker = Some(Worker {
            stop: stop_sender,
            handle,
        });
    }

    pub async fn shutdown(&self) {
        if let Some(worker) = self.worker.lock().await.take() {
            stop(worker).await;
        }
    }

    pub async fn is_running(&self) -> bool {
        self.worker.lock().await.is_some()
    }

    /// Look at every site once: order what is missing, renew what is close.
    pub async fn sweep(&self, wanted: &[Wanted]) {
        for one in wanted {
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
        let issued = self.issuer.issue(&wanted.host).await?;
        let (chain, key) = self.issuer.paths(&wanted.host);
        // Written before it is served, so a restart a second later still has
        // it. A failure to write is worth knowing about but not worth
        // discarding a certificate over.
        if let Err(error) = write_pair(&chain, &key, &issued.chain_pem, &issued.key_pem) {
            tracing::warn!(
                target: "gatel::certificate",
                event = "certificate_not_saved",
                site = %wanted.host,
                detail = %error,
            );
        }
        self.store.put(wanted.site.clone(), issued.certificate);
        self.next_attempt.lock().await.remove(&wanted.host);
        tracing::info!(
            target: "gatel::certificate",
            event = "certificate_issued",
            site = %wanted.host,
            source = "acme",
        );
        Ok(())
    }

    /// When the stored certificate for a name expires, in unix seconds.
    fn not_after(&self, host: &str) -> Option<i64> {
        let (chain, key) = self.issuer.paths(host);
        let chain_pem = std::fs::read(chain).ok()?;
        let key_pem = std::fs::read(key).ok()?;
        certon::Certificate::from_pem(&chain_pem, &key_pem)
            .ok()
            .map(|parsed| parsed.not_after.timestamp())
    }
}

fn write_pair(chain: &Path, key: &Path, chain_pem: &[u8], key_pem: &[u8]) -> Result<(), String> {
    std::fs::write(chain, chain_pem)
        .map_err(|error| format!("cannot write `{}`: {error}", chain.display()))?;
    std::fs::write(key, key_pem).map_err(|error| format!("cannot write `{}`: {error}", key.display()))?;
    restrict(key);
    Ok(())
}

/// A private key is readable by its owner and nobody else.
#[cfg(unix)]
fn restrict(key: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_key: &Path) {}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

async fn stop(worker: Worker) {
    let _ = worker.stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), worker.handle).await;
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
    async fn a_site_with_no_certificate_is_reported_as_pending() {
        let root = directory("pending");
        let acme = manager(&root);
        acme.sweep(&[wanted("example.com")]).await;

        // Degraded, not down: this instance is still serving everything else,
        // and every replica would say the same thing at the same moment.
        let report = acme.telemetry.health.report();
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
            !acme.due(&sites[0].site, "example.com").await,
            "a second attempt must wait"
        );
    }

    #[tokio::test]
    async fn a_name_nothing_was_ordered_for_is_due() {
        let root = directory("due");
        let acme = manager(&root);
        assert!(acme.due(&ResourceId::new(ResourceKind::Site, "a"), "a").await);
    }

    #[tokio::test]
    async fn nothing_on_disk_is_nothing_to_adopt() {
        let root = directory("adopt-missing");
        let acme = manager(&root);
        assert!(!acme.adopt(&ResourceId::new(ResourceKind::Site, "a"), "a.example.com"));
        assert!(acme.store.is_empty());
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
            acme.telemetry
                .health
                .report()
                .reasons
                .iter()
                .any(|reason| reason.code() == "certificate_pending")
        );

        acme.reconcile(Vec::new()).await;
        assert!(
            !acme
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
