//! A site's TLS declaration.
//!
//! Split out because it is a whole subject of its own — manual certificates,
//! the development authority, ACME, and client authentication — and because
//! everything around it in `normalize` is about routes and names.

use gatel_model::{Code, Diagnostic, Node, SecretRef};

use super::Context;
use crate::spec::{CertificateSource, ClientAuth, SiteTls};

impl Context<'_> {
    pub(super) fn site_tls(&mut self, node: &Node) -> Option<SiteTls> {
        let secret = |context: &mut Self, name: &str| -> Option<SecretRef> {
            let property = node.prop(name)?;
            let text = property.value.as_str()?;
            match SecretRef::parse(text) {
                Ok(reference) => Some(reference),
                Err(message) => {
                    context
                        .diagnostics
                        .error(Code::InvalidSecret, property.origin.clone(), message);
                    None
                }
            }
        };

        let keyword = node.args.first().and_then(|value| value.as_str());
        let source = match keyword {
            Some("internal") => CertificateSource::Internal,
            Some("acme") => CertificateSource::Acme,
            Some(other) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.args[0].origin.clone(),
                        format!("unknown certificate source `{other}`"),
                    )
                    .with_help("write `tls internal`, `tls acme`, or `tls cert=\"..\" key=\"..\"`"),
                );
                return None;
            }
            None => {
                let cert = secret(self, "cert");
                let key = secret(self, "key");
                match (cert, key) {
                    (Some(cert), Some(key)) => CertificateSource::Manual { cert, key },
                    _ => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                Code::MissingField,
                                node.origin.clone(),
                                "`tls` needs both `cert` and `key`, or one of `internal` / `acme`",
                            )
                            .with_help("certificate material is referenced, never inlined"),
                        );
                        return None;
                    }
                }
            }
        };

        let client_auth = match node.child("client-auth") {
            Some(child) => {
                let Some(ca) = child.prop("ca").and_then(|property| property.value.as_str()) else {
                    self.error(Code::MissingField, &child.origin, "`client-auth` requires a `ca`");
                    return None;
                };
                let ca = match SecretRef::parse(ca) {
                    Ok(reference) => reference,
                    Err(message) => {
                        self.error(Code::InvalidSecret, &child.origin, message);
                        return None;
                    }
                };
                let required = child
                    .args
                    .first()
                    .and_then(|value| value.as_str())
                    .map(|mode| mode == "required")
                    .unwrap_or(true);
                Some(ClientAuth { required, ca })
            }
            None => None,
        };

        for property in &node.props {
            if !["cert", "key"].contains(&property.name.as_str()) {
                self.error(
                    Code::UnknownProperty,
                    &property.origin,
                    format!("`tls` has no property `{}`", property.name),
                );
            }
        }
        Some(SiteTls { source, client_auth })
    }
}
