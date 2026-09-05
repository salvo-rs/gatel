//! Speaking TLS to a backend.
//!
//! The other direction from the rest of this crate, and the same rule applies:
//! everything that can fail happens during prepare. A certificate authority
//! that will not load, or a client identity whose key does not match its
//! certificate, stops a reload — rather than turning into a connection error
//! under traffic, where it looks like the backend's fault.
//!
//! There is no way to turn verification off. That option exists in other
//! gateways because it makes a broken deployment work today, and it is how
//! deployments stay broken for years. A private authority is named with `ca`,
//! and a name that does not match is a real problem worth seeing.

use std::path::Path;
use std::sync::Arc;

use gatel_model::SecretRef;
use gatel_plan::{UpstreamProtocol, UpstreamTls};
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// How to reach one upstream over TLS.
pub struct ClientTls {
    connector: TlsConnector,
    /// The name presented in SNI and checked against the certificate. Written
    /// in the configuration rather than taken from the address, because a
    /// backend is usually reached by an address that is not its name.
    server_name: ServerName<'static>,
    described: String,
}

impl std::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The description names what is reached, never the files behind it.
        f.debug_struct("ClientTls")
            .field("server_name", &self.described)
            .finish_non_exhaustive()
    }
}

impl ClientTls {
    /// Build the configuration for one upstream.
    ///
    /// `root` is the directory `file:` references resolve against, so a
    /// relative path means the same thing wherever the process was started.
    pub fn new(spec: &UpstreamTls, protocol: UpstreamProtocol, root: &Path) -> Result<Self, String> {
        let server_name = ServerName::try_from(spec.server_name.clone())
            .map_err(|_| format!("`{}` is not a name a certificate can carry", spec.server_name))?;

        let roots = match &spec.ca {
            Some(ca) => private_roots(ca, root)?,
            // The platform's public authorities, compiled in rather than read
            // from the machine: a gateway should reach the same verdict on a
            // container with no trust store as on a workstation.
            None => RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            },
        };

        let builder = ClientConfig::builder().with_root_certificates(roots);
        let mut config = match (&spec.client_cert, &spec.client_key) {
            (Some(cert), Some(key)) => {
                let (chain, key) = identity(cert, key, root)?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|error| format!("the client identity is not usable: {error}"))?
            }
            (None, None) => builder.with_no_client_auth(),
            // Decoding refuses this, so reaching it means the two disagree.
            _ => return Err("a client identity needs both `client-cert` and `client-key`".to_string()),
        };
        // Offer only what the pool will actually speak. A backend that picked
        // the other one would get a connection nothing knows how to drive.
        config.alpn_protocols = match protocol {
            UpstreamProtocol::Http1 => vec![b"http/1.1".to_vec()],
            UpstreamProtocol::Http2 => vec![b"h2".to_vec()],
        };

        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
            described: describe(spec),
        })
    }

    /// Wrap a connected socket in TLS.
    pub async fn connect<IO>(&self, stream: IO) -> Result<TlsStream<IO>, String>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        self.connector
            .connect(self.server_name.clone(), stream)
            .await
            .map_err(|error| format!("TLS to `{}` failed: {error}", self.described))
    }

    pub fn server_name(&self) -> String {
        self.described.clone()
    }
}

fn describe(spec: &UpstreamTls) -> String {
    let mut described = spec.server_name.clone();
    if spec.ca.is_some() {
        described.push_str(" (private authority)");
    }
    if spec.client_cert.is_some() {
        described.push_str(" (with a client identity)");
    }
    described
}

/// A private authority replaces the public ones rather than adding to them.
///
/// Adding would mean a backend could be certified by anybody, which is not what
/// naming one authority asks for.
fn private_roots(ca: &SecretRef, root: &Path) -> Result<RootCertStore, String> {
    let path = ca.resolve_path(root)?;
    let bytes = std::fs::read(&path).map_err(|error| format!("cannot read the upstream CA: {error}"))?;
    let certificates = crate::certificate::read_chain(&bytes, "upstream CA")?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| format!("the upstream CA is not usable: {error}"))?;
    }
    Ok(roots)
}

fn identity(
    cert: &SecretRef,
    key: &SecretRef,
    root: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let cert_bytes = std::fs::read(cert.resolve_path(root)?)
        .map_err(|error| format!("cannot read the client certificate: {error}"))?;
    let key_bytes = std::fs::read(key.resolve_path(root)?)
        .map_err(|error| format!("cannot read the client key: {error}"))?;

    let chain = crate::certificate::read_chain(&cert_bytes, "client certificate")?;
    let key = PrivateKeyDer::from_pem_slice(&key_bytes).map_err(|error| match error {
        pem::Error::NoItemsFound => "the client key file contains no private key".to_string(),
        error => format!("cannot read the client key: {error}"),
    })?;
    Ok((chain, key))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn directory(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gatel-client-tls-{tag}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn spec(server_name: &str) -> UpstreamTls {
        UpstreamTls {
            server_name: server_name.to_string(),
            ca: None,
            client_cert: None,
            client_key: None,
        }
    }

    #[test]
    fn the_public_authorities_are_used_when_none_is_named() {
        let root = directory("public");
        assert!(ClientTls::new(&spec("api.example.com"), UpstreamProtocol::Http1, &root).is_ok());
    }

    #[test]
    fn a_name_a_certificate_could_not_carry_is_refused() {
        let root = directory("badname");
        let error = ClientTls::new(&spec("not a name"), UpstreamProtocol::Http1, &root).unwrap_err();
        assert!(error.contains("not a name a certificate can carry"), "{error}");
    }

    #[test]
    fn a_private_authority_is_read_during_prepare() {
        let root = directory("ca");
        let generated = rcgen::generate_simple_self_signed(vec!["api.internal".to_string()]).unwrap();
        std::fs::write(root.join("ca.pem"), generated.cert.pem()).unwrap();

        let mut spec = spec("api.internal");
        spec.ca = Some(SecretRef::parse("ca.pem").unwrap());
        assert!(ClientTls::new(&spec, UpstreamProtocol::Http1, &root).is_ok());
    }

    #[test]
    fn a_missing_authority_stops_the_reload_rather_than_a_connection() {
        let root = directory("missing-ca");
        let mut spec = spec("api.internal");
        spec.ca = Some(SecretRef::parse("absent.pem").unwrap());
        let error = ClientTls::new(&spec, UpstreamProtocol::Http1, &root).unwrap_err();
        assert!(error.contains("cannot read the upstream CA"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_certificate_is_refused() {
        let root = directory("nonsense-ca");
        std::fs::write(root.join("ca.pem"), "this is not a certificate\n").unwrap();
        let mut spec = spec("api.internal");
        spec.ca = Some(SecretRef::parse("ca.pem").unwrap());
        let error = ClientTls::new(&spec, UpstreamProtocol::Http1, &root).unwrap_err();
        assert!(error.contains("contains no certificate"), "{error}");
    }

    #[test]
    fn a_client_identity_is_loaded_and_checked() {
        let root = directory("identity");
        let generated = rcgen::generate_simple_self_signed(vec!["gateway".to_string()]).unwrap();
        std::fs::write(root.join("client.pem"), generated.cert.pem()).unwrap();
        crate::certificate::write_key(
            &root.join("client-key.pem"),
            &generated.signing_key.serialize_pem(),
        );

        let mut spec = spec("api.internal");
        spec.client_cert = Some(SecretRef::parse("client.pem").unwrap());
        spec.client_key = Some(SecretRef::parse("client-key.pem").unwrap());
        assert!(ClientTls::new(&spec, UpstreamProtocol::Http1, &root).is_ok());
    }

    #[test]
    fn a_client_key_that_does_not_match_is_refused_before_any_traffic() {
        let root = directory("mismatch");
        let certificate = rcgen::generate_simple_self_signed(vec!["gateway".to_string()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["gateway".to_string()]).unwrap();
        std::fs::write(root.join("client.pem"), certificate.cert.pem()).unwrap();
        crate::certificate::write_key(&root.join("client-key.pem"), &other.signing_key.serialize_pem());

        let mut spec = spec("api.internal");
        spec.client_cert = Some(SecretRef::parse("client.pem").unwrap());
        spec.client_key = Some(SecretRef::parse("client-key.pem").unwrap());
        let error = ClientTls::new(&spec, UpstreamProtocol::Http1, &root).unwrap_err();
        assert!(error.contains("client identity is not usable"), "{error}");
    }

    #[test]
    fn what_is_offered_matches_what_the_pool_can_drive() {
        let root = directory("alpn");
        // A backend that chose the other protocol would leave a connection
        // nothing knows how to speak.
        for protocol in [UpstreamProtocol::Http1, UpstreamProtocol::Http2] {
            assert!(
                ClientTls::new(&spec("api.example.com"), protocol, &root).is_ok(),
                "{protocol:?} should be offered"
            );
        }
    }

    #[test]
    fn what_it_reaches_is_described_without_naming_a_file() {
        let root = directory("describe");
        let generated = rcgen::generate_simple_self_signed(vec!["api.internal".to_string()]).unwrap();
        std::fs::write(root.join("ca.pem"), generated.cert.pem()).unwrap();
        let mut spec = spec("api.internal");
        spec.ca = Some(SecretRef::parse("ca.pem").unwrap());
        let tls = ClientTls::new(&spec, UpstreamProtocol::Http1, &root).unwrap();
        let described = tls.server_name();
        assert!(described.contains("api.internal"), "{described}");
        assert!(described.contains("private authority"), "{described}");
        assert!(!described.contains("ca.pem"), "{described}");
    }
}
