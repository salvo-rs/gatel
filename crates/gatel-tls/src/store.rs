//! Where certificates live.
//!
//! Not in the plan: a certificate is renewed on its own schedule, and a renewal
//! is not something the operator asked for. Putting it in the plan would mean
//! every renewal produced a generation, and every reload risked losing one.
//!
//! The store is shared across generations. A handshake reads the current
//! certificate through the resolver, so replacing one takes effect on the next
//! handshake and leaves established connections alone.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use gatel_model::ResourceId;
use gatel_plan::HostMatch;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::certificate::Certificate;

/// Certificates, keyed by the site that owns them.
#[derive(Default)]
pub struct CertificateStore {
    entries: RwLock<BTreeMap<ResourceId, Certificate>>,
}

impl std::fmt::Debug for CertificateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("CertificateStore")
            .field("sites", &entries.keys().map(ResourceId::name).collect::<Vec<_>>())
            .finish()
    }
}

impl CertificateStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, site: &ResourceId) -> Option<Certificate> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(site)
            .cloned()
    }

    /// Install or replace a site's certificate.
    ///
    /// Returns whether anything changed, so a caller can log a renewal without
    /// logging every reload.
    pub fn put(&self, site: ResourceId, certificate: Certificate) -> bool {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let changed = entries
            .get(&site)
            .is_none_or(|existing| existing.certified.cert != certificate.certified.cert);
        entries.insert(site, certificate);
        changed
    }

    /// Forget the certificates of sites that no longer exist.
    pub fn retain(&self, sites: &[ResourceId]) -> Vec<ResourceId> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let removed: Vec<ResourceId> = entries
            .keys()
            .filter(|site| !sites.contains(site))
            .cloned()
            .collect();
        for site in &removed {
            entries.remove(site);
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Picks a certificate for a handshake.
///
/// The site list belongs to one generation and never changes; the certificates
/// behind it can, which is what lets a renewal land without a reload.
pub struct SniResolver {
    store: Arc<CertificateStore>,
    /// Most specific host first, matching the order routing uses.
    sites: Vec<(HostMatch, ResourceId)>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniResolver")
            .field(
                "sites",
                &self
                    .sites
                    .iter()
                    .map(|(host, _)| host.describe())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SniResolver {
    pub fn new(store: Arc<CertificateStore>, mut sites: Vec<(HostMatch, ResourceId)>) -> Self {
        sites.sort_by_key(|(host, _)| std::cmp::Reverse(host.specificity()));
        Self { store, sites }
    }

    /// The site a name belongs to, by the same rules routing uses, so a request
    /// cannot be handed a certificate for one site and then routed to another.
    fn site_for(&self, name: Option<&str>) -> Option<&ResourceId> {
        match name {
            Some(name) => {
                let lowered = name.to_ascii_lowercase();
                self.sites
                    .iter()
                    .find(|(host, _)| host.matches(&lowered))
                    .map(|(_, site)| site)
            }
            // No SNI: an old client, or something connecting by address. Only a
            // catch-all site can answer, because guessing would hand out a
            // certificate for a name the client never asked for.
            None => self
                .sites
                .iter()
                .find(|(host, _)| matches!(host, HostMatch::Any))
                .map(|(_, site)| site),
        }
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let site = self.site_for(client_hello.server_name())?;
        // Reading through the store rather than capturing the certificate is
        // the whole point: a renewal is visible to the next handshake.
        let certificate = self.store.get(site)?;
        Some(certificate.certified)
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::ResourceKind;

    use super::*;

    fn site(name: &str) -> ResourceId {
        ResourceId::new(ResourceKind::Site, name)
    }

    fn certificate(name: &str) -> Certificate {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        Certificate::from_pem(
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
            name,
        )
        .unwrap()
    }

    fn resolver(hosts: &[&str]) -> (Arc<CertificateStore>, SniResolver) {
        let store = Arc::new(CertificateStore::new());
        let mut sites = Vec::new();
        for host in hosts {
            let id = site(host);
            store.put(
                id.clone(),
                certificate(if *host == "*" { "fallback" } else { host }),
            );
            sites.push((HostMatch::parse(host).unwrap(), id));
        }
        (Arc::clone(&store), SniResolver::new(store, sites))
    }

    #[test]
    fn a_name_resolves_to_its_own_site() {
        let (_, resolver) = resolver(&["a.example.com", "b.example.com"]);
        assert_eq!(
            resolver.site_for(Some("a.example.com")),
            Some(&site("a.example.com"))
        );
        assert_eq!(
            resolver.site_for(Some("b.example.com")),
            Some(&site("b.example.com"))
        );
        assert_eq!(resolver.site_for(Some("c.example.com")), None);
    }

    #[test]
    fn an_exact_site_wins_over_a_wildcard() {
        let (_, resolver) = resolver(&["*.example.com", "api.example.com"]);
        assert_eq!(
            resolver.site_for(Some("api.example.com")),
            Some(&site("api.example.com"))
        );
        assert_eq!(
            resolver.site_for(Some("other.example.com")),
            Some(&site("*.example.com"))
        );
    }

    #[test]
    fn a_client_without_sni_only_reaches_a_catch_all() {
        let (_, named) = resolver(&["api.example.com"]);
        assert_eq!(
            named.site_for(None),
            None,
            "guessing would hand out the wrong certificate"
        );

        let (_, catch_all) = resolver(&["api.example.com", "*"]);
        assert_eq!(catch_all.site_for(None), Some(&site("*")));
    }

    #[test]
    fn sni_is_matched_case_insensitively() {
        let (_, resolver) = resolver(&["api.example.com"]);
        assert_eq!(
            resolver.site_for(Some("API.Example.COM")),
            Some(&site("api.example.com"))
        );
    }

    #[test]
    fn replacing_a_certificate_is_visible_without_rebuilding_the_resolver() {
        let (store, resolver) = resolver(&["api.example.com"]);
        let id = site("api.example.com");
        let before = store.get(&id).unwrap();

        assert!(
            store.put(id.clone(), certificate("api.example.com")),
            "a new key is a change"
        );
        let after = store.get(&id).unwrap();
        assert_ne!(before.certified.cert, after.certified.cert);
        // The resolver still points at the same site, and now yields the new
        // certificate — which is what a renewal looks like.
        assert_eq!(resolver.site_for(Some("api.example.com")), Some(&id));
    }

    #[test]
    fn installing_the_same_certificate_twice_is_not_a_change() {
        let store = CertificateStore::new();
        let id = site("api.example.com");
        let certificate = certificate("api.example.com");
        assert!(store.put(id.clone(), certificate.clone()));
        assert!(
            !store.put(id, certificate),
            "a reload must not look like a renewal"
        );
    }

    #[test]
    fn retaining_forgets_only_removed_sites() {
        let (store, _) = resolver(&["a.example.com", "b.example.com"]);
        let removed = store.retain(&[site("a.example.com")]);
        assert_eq!(removed, vec![site("b.example.com")]);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_site_with_no_certificate_yet_resolves_to_nothing_rather_than_a_wrong_one() {
        let store = Arc::new(CertificateStore::new());
        let resolver = SniResolver::new(
            Arc::clone(&store),
            vec![(
                HostMatch::parse("api.example.com").unwrap(),
                site("api.example.com"),
            )],
        );
        assert!(resolver.site_for(Some("api.example.com")).is_some());
        assert!(
            store.get(&site("api.example.com")).is_none(),
            "the handshake must fail, not guess"
        );
    }
}
