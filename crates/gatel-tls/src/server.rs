//! Building a listener's TLS configuration.
//!
//! One immutable `ServerConfig` per generation. Certificates are reached
//! through the store, so the configuration a connection handshakes against is
//! fixed while the certificate behind it can still be renewed.

use std::path::Path;
use std::sync::Arc;

use gatel_model::{ResourceId, SecretRef};
use gatel_plan::{CertificateSource, HostMatch, ListenerPlan, SiteTls};
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;

use crate::certificate::Certificate;
use crate::internal::InternalCa;
use crate::store::{CertificateStore, SniResolver};

/// What a listener needs to terminate TLS.
pub struct TlsPreparation<'a> {
    pub store: &'a Arc<CertificateStore>,
    /// Created lazily: a configuration that never says `internal` should not
    /// leave a certificate authority on the machine.
    pub internal: &'a dyn Fn() -> Result<Arc<InternalCa>, String>,
    /// Where `file:` references resolve against.
    pub root: &'a Path,
    /// Opened only when a site asks for an automatic certificate, and then once
    /// for the process: the account with the authority is not per generation.
    pub acme: &'a dyn Fn(&gatel_plan::Acme) -> Result<Arc<crate::acme::Acme>, String>,
}

/// Build the TLS configuration for one listener, loading every certificate it
/// needs.
///
/// Returns `None` for a plaintext listener. Every failure here happens during
/// prepare, so a certificate that will not load stops a reload rather than
/// breaking a handshake later.
pub fn prepare_listener(
    listener: &ListenerPlan,
    acme_config: Option<&gatel_plan::Acme>,
    preparation: &TlsPreparation<'_>,
) -> Result<Option<Arc<ServerConfig>>, String> {
    if !listener.listener.protocol.is_tls() {
        return Ok(None);
    }

    let mut sites: Vec<(HostMatch, ResourceId)> = Vec::new();
    let mut client_auth: Option<(ResourceId, SecretRef, bool)> = None;

    for site in &listener.sites {
        let Some(tls) = &site.tls else {
            // A site with no certificate cannot be served over TLS. Saying so
            // during prepare is better than a handshake failure per request.
            return Err(format!(
                "site `{}` is served on the TLS listener `{}` but has no `tls`",
                site.host.describe(),
                listener.listener.name
            ));
        };
        load_certificate(site.id.clone(), &site.host, tls, acme_config, preparation)?;
        sites.push((site.host.clone(), site.id.clone()));

        if let Some(auth) = &tls.client_auth {
            if let Some((existing, ..)) = &client_auth {
                return Err(format!(
                    "listener `{}` cannot use two client-authentication policies (`{}` and `{}`)",
                    listener.listener.name,
                    existing.name(),
                    site.id.name()
                ));
            }
            client_auth = Some((site.id.clone(), auth.ca.clone(), auth.required));
        }
    }

    if sites.is_empty() {
        return Err(format!(
            "TLS listener `{}` serves no sites",
            listener.listener.name
        ));
    }

    let resolver = Arc::new(SniResolver::new(Arc::clone(preparation.store), sites));
    let builder = ServerConfig::builder();

    let mut config = match client_auth {
        None => builder.with_no_client_auth().with_cert_resolver(resolver),
        Some((_, ca, required)) => {
            let roots = load_roots(&ca, preparation.root)?;
            let verifier = if required {
                WebPkiClientVerifier::builder(Arc::new(roots))
            } else {
                // Optional still verifies a certificate that is offered; it
                // only allows a client that offers none.
                WebPkiClientVerifier::builder(Arc::new(roots)).allow_unauthenticated()
            }
            .build()
            .map_err(|error| format!("cannot build the client verifier: {error}"))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_cert_resolver(resolver)
        }
    };

    // Offer HTTP/2 first: a client that supports it should get it, and one that
    // does not falls back without a round trip.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(Arc::new(config)))
}

fn load_certificate(
    site: ResourceId,
    host: &HostMatch,
    tls: &SiteTls,
    acme_config: Option<&gatel_plan::Acme>,
    preparation: &TlsPreparation<'_>,
) -> Result<(), String> {
    match &tls.source {
        CertificateSource::Manual { cert, key } => {
            let cert_path = cert.resolve_path(preparation.root)?;
            let key_path = key.resolve_path(preparation.root)?;
            let certificate = Certificate::from_pem_files(&cert_path, &key_path)
                .map_err(|error| format!("site `{}`: {error}", host.describe()))?;
            if preparation.store.put(site, certificate) {
                tracing::info!(
                    target: "gatel::certificate",
                    event = "certificate_loaded",
                    site = %host.describe(),
                    source = "manual",
                );
            }
        }
        CertificateSource::Internal => {
            // `*` has no name at all; `*.localhost` does, and our own authority
            // can issue a wildcard for it.
            let name = match host {
                HostMatch::Exact(name) => name.clone(),
                HostMatch::Suffix(suffix) => format!("*{suffix}"),
                HostMatch::Any => {
                    return Err(
                        "site `*` cannot use `tls internal`: there is no host to issue for".to_string(),
                    );
                }
            };
            let name = &name;
            // Only issue once per process: a new certificate on every reload
            // would make browsers re-prompt and would defeat session resumption.
            if preparation.store.get(&site).is_none() {
                let authority = (preparation.internal)()?;
                let certificate = authority.issue(name)?;
                preparation.store.put(site, certificate);
                tracing::info!(
                    target: "gatel::certificate",
                    event = "certificate_issued",
                    site = %name,
                    source = "internal",
                );
            }
        }
        CertificateSource::Acme => {
            // Normalize already refused a site with no single name.
            let HostMatch::Exact(name) = host else {
                return Err(format!(
                    "site `{}` cannot use `tls acme`: there is no single host to certify",
                    host.describe()
                ));
            };
            let Some(config) = acme_config else {
                // `normalize` refuses this, so reaching it means the plan and
                // the spec disagree.
                return Err(format!(
                    "site `{}` asks for `tls acme` with no certificate authority configured",
                    host.describe()
                ));
            };
            let acme = (preparation.acme)(config)?;
            // Whatever a previous run obtained, so a restart serves at once.
            // Nothing is ordered here: an order takes seconds when everything
            // works and minutes when it does not, and a reload must not wait
            // for a certificate authority's bad afternoon.
            if preparation.store.get(&site).is_none() && !acme.adopt(&site, name) {
                // Deliberately not a placeholder certificate. One this process
                // signed itself would be a certificate that lies, and teaching
                // people to click through a warning is worse than a handshake
                // that fails while readiness says exactly why.
                tracing::info!(
                    target: "gatel::certificate",
                    event = "certificate_pending",
                    site = %name,
                    source = "acme",
                );
            }
        }
    }
    Ok(())
}

fn load_roots(ca: &SecretRef, root: &Path) -> Result<rustls::RootCertStore, String> {
    let path = ca.resolve_path(root)?;
    let bytes = std::fs::read(&path).map_err(|error| format!("cannot read the client CA: {error}"))?;
    let certificates: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::Cursor::new(bytes))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("cannot read the client CA: {error}"))?;
    if certificates.is_empty() {
        return Err("the client CA file contains no certificate".to_string());
    }
    let mut roots = rustls::RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| format!("the client CA is not usable: {error}"))?;
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, ResourceKind, SourceId};
    use gatel_plan::{Listener, ListenerProtocol, SitePlan};

    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gatel-tls-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn listener(protocol: ListenerProtocol, sites: Vec<SitePlan>) -> ListenerPlan {
        ListenerPlan {
            listener: Listener {
                id: ResourceId::new(ResourceKind::Listener, "https"),
                owner: SourceId::file("main"),
                name: "https".into(),
                bind: "127.0.0.1:443".parse().unwrap(),
                protocol,
                origin: Origin::synthetic("test"),
            },
            sites,
        }
    }

    fn site(host: &str, tls: Option<SiteTls>) -> SitePlan {
        SitePlan {
            fallback: Vec::new(),
            id: ResourceId::new(ResourceKind::Site, host),
            host: HostMatch::parse(host).unwrap(),
            tls,
            routes: Vec::new(),
        }
    }

    struct Fixture {
        store: Arc<CertificateStore>,
        directory: std::path::PathBuf,
        ca_directory: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            Self {
                store: Arc::new(CertificateStore::new()),
                directory: temp(tag),
                ca_directory: temp(&format!("{tag}-ca")),
            }
        }

        fn prepare(&self, listener: &ListenerPlan) -> Result<Option<Arc<ServerConfig>>, String> {
            self.prepare_with(listener, None)
        }

        fn prepare_with(
            &self,
            listener: &ListenerPlan,
            acme_config: Option<&gatel_plan::Acme>,
        ) -> Result<Option<Arc<ServerConfig>>, String> {
            let ca_directory = self.ca_directory.clone();
            let internal = move || InternalCa::load_or_create(&ca_directory).map(Arc::new);
            let directory = self.directory.clone();
            let store = Arc::clone(&self.store);
            let challenges = crate::acme::Challenges::new();
            let acme = move |config: &gatel_plan::Acme| {
                crate::acme::Acme::new(
                    config,
                    &directory,
                    Arc::clone(&challenges),
                    Arc::clone(&store),
                    gatel_observe::Telemetry::new(),
                )
            };
            prepare_listener(
                listener,
                acme_config,
                &TlsPreparation {
                    store: &self.store,
                    internal: &internal,
                    acme: &acme,
                    root: &self.directory,
                },
            )
        }

        /// Write a certificate pair into the configuration directory.
        fn write_pair(&self, name: &str) -> SiteTls {
            let generated = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
            std::fs::write(self.directory.join("cert.pem"), generated.cert.pem()).unwrap();
            std::fs::write(
                self.directory.join("key.pem"),
                generated.signing_key.serialize_pem(),
            )
            .unwrap();
            SiteTls {
                source: CertificateSource::Manual {
                    cert: SecretRef::parse("cert.pem").unwrap(),
                    key: SecretRef::parse("key.pem").unwrap(),
                },
                client_auth: None,
                automatic: false,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
            let _ = std::fs::remove_dir_all(&self.ca_directory);
        }
    }

    #[test]
    fn a_plaintext_listener_needs_no_configuration() {
        let fixture = Fixture::new("plain");
        let listener = listener(ListenerProtocol::Http, vec![site("*", None)]);
        assert!(fixture.prepare(&listener).unwrap().is_none());
    }

    #[test]
    fn a_manual_certificate_is_loaded_during_prepare() {
        let fixture = Fixture::new("manual");
        let tls = fixture.write_pair("api.example.com");
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", Some(tls))]);

        let config = fixture.prepare(&listener).unwrap().unwrap();
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert_eq!(fixture.store.len(), 1);
    }

    #[test]
    fn a_missing_certificate_file_fails_the_reload_rather_than_a_handshake() {
        let fixture = Fixture::new("missing");
        let tls = SiteTls {
            source: CertificateSource::Manual {
                cert: SecretRef::parse("absent.pem").unwrap(),
                key: SecretRef::parse("absent-key.pem").unwrap(),
            },
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", Some(tls))]);
        let error = fixture.prepare(&listener).unwrap_err();
        assert!(error.contains("api.example.com"), "{error}");
    }

    #[test]
    fn a_tls_listener_serving_a_site_without_a_certificate_is_refused() {
        let fixture = Fixture::new("nocert");
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", None)]);
        let error = fixture.prepare(&listener).unwrap_err();
        assert!(error.contains("has no `tls`"), "{error}");
    }

    #[test]
    fn the_internal_authority_issues_for_a_named_site() {
        let fixture = Fixture::new("internal");
        let tls = SiteTls {
            source: CertificateSource::Internal,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("app.localhost", Some(tls))]);

        assert!(fixture.prepare(&listener).unwrap().is_some());
        assert_eq!(fixture.store.len(), 1);
        let issued = fixture
            .store
            .get(&ResourceId::new(ResourceKind::Site, "app.localhost"))
            .unwrap();
        assert_eq!(issued.source, "internal:app.localhost");
    }

    #[test]
    fn reloading_does_not_reissue_an_internal_certificate() {
        let fixture = Fixture::new("reissue");
        let tls = SiteTls {
            source: CertificateSource::Internal,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("app.localhost", Some(tls))]);

        fixture.prepare(&listener).unwrap();
        let first = fixture
            .store
            .get(&ResourceId::new(ResourceKind::Site, "app.localhost"))
            .unwrap();
        fixture.prepare(&listener).unwrap();
        let second = fixture
            .store
            .get(&ResourceId::new(ResourceKind::Site, "app.localhost"))
            .unwrap();
        assert_eq!(
            first.certified.cert, second.certified.cert,
            "a reload is not a renewal"
        );
    }

    #[test]
    fn a_catch_all_site_cannot_use_the_internal_authority() {
        // `*` is not a name, so there is nothing to put in a certificate.
        let fixture = Fixture::new("wildcard");
        let tls = SiteTls {
            source: CertificateSource::Internal,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("*", Some(tls))]);
        let error = fixture.prepare(&listener).unwrap_err();
        assert!(error.contains("no host to issue for"), "{error}");
    }

    #[test]
    fn a_wildcard_name_gets_a_wildcard_certificate() {
        // `*.localhost` is a name, and our own authority can certify it — which
        // a public one never would.
        let fixture = Fixture::new("wildcard-name");
        let tls = SiteTls {
            source: CertificateSource::Internal,
            client_auth: None,
            automatic: true,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("*.localhost", Some(tls))]);
        assert!(fixture.prepare(&listener).unwrap().is_some());
        assert_eq!(fixture.store.len(), 1);
    }

    #[test]
    fn acme_prepares_without_reaching_a_certificate_authority() {
        // A listener whose site has no certificate yet still starts. Ordering
        // one is background work, and a reload that waited for a certificate
        // authority would be a reload its bad afternoon could stall.
        let fixture = Fixture::new("acme");
        let tls = SiteTls {
            source: CertificateSource::Acme,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", Some(tls))]);
        let config = acme_config();
        let prepared = fixture.prepare_with(&listener, Some(&config)).unwrap();
        assert!(prepared.is_some(), "the listener is ready to accept");
        assert!(
            fixture.store.is_empty(),
            "and has nothing to offer for that name until an order finishes"
        );
    }

    #[test]
    fn acme_without_a_certificate_authority_is_an_inconsistency_not_a_guess() {
        // `normalize` refuses this, so reaching it means the spec and the plan
        // disagree — which is worth saying rather than defaulting to a CA.
        let fixture = Fixture::new("acme-nowhere");
        let tls = SiteTls {
            source: CertificateSource::Acme,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", Some(tls))]);
        let error = fixture.prepare_with(&listener, None).unwrap_err();
        assert!(error.contains("no certificate authority configured"), "{error}");
    }

    #[test]
    fn a_certificate_a_previous_run_obtained_is_served_at_once() {
        let fixture = Fixture::new("acme-adopt");
        let config = acme_config();
        // What a previous run would have left behind.
        let generated = rcgen::generate_simple_self_signed(vec!["api.example.com".to_string()]).unwrap();
        let state = fixture.directory.join("acme");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("api.example.com.crt"), generated.cert.pem()).unwrap();
        std::fs::write(
            state.join("api.example.com.key"),
            generated.signing_key.serialize_pem(),
        )
        .unwrap();

        let tls = SiteTls {
            source: CertificateSource::Acme,
            client_auth: None,
            automatic: false,
        };
        let listener = listener(ListenerProtocol::Https, vec![site("api.example.com", Some(tls))]);
        fixture.prepare_with(&listener, Some(&config)).unwrap();
        assert_eq!(
            fixture.store.len(),
            1,
            "a restart serves what it had rather than waiting to be told again"
        );
    }

    fn acme_config() -> gatel_plan::Acme {
        gatel_plan::Acme {
            directory: "http://127.0.0.1:1/directory".to_string(),
            email: "ops@example.com".to_string(),
            agreed: true,
            directory_path: None,
            renew_before: std::time::Duration::from_secs(86_400),
        }
    }

    #[test]
    fn two_client_auth_policies_on_one_listener_are_refused() {
        let fixture = Fixture::new("twoauth");
        let mut first = fixture.write_pair("a.example.com");
        let mut second = fixture.write_pair("b.example.com");
        let auth = gatel_plan::ClientAuth {
            required: true,
            ca: SecretRef::parse("cert.pem").unwrap(),
        };
        first.client_auth = Some(auth.clone());
        second.client_auth = Some(auth);

        let listener = listener(
            ListenerProtocol::Https,
            vec![
                site("a.example.com", Some(first)),
                site("b.example.com", Some(second)),
            ],
        );
        let error = fixture.prepare(&listener).unwrap_err();
        assert!(error.contains("two client-authentication policies"), "{error}");
    }
}
