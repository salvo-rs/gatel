//! Handing a connection over once both ends agreed to stop speaking HTTP.
//!
//! A 101 is where the proxy stops being a proxy. The connection leaves the
//! pool — it can never carry another request — and both halves are spliced
//! together for as long as the upgrade timeouts allow.

use salvo_core::http::{Response, StatusCode};

use super::ProxyTerminal;
use super::outbound::HOP_BY_HOP;
use crate::upgrade;

impl ProxyTerminal {
    /// Answer the client with the backend's 101 and hand both halves to the
    /// tunnel once hyper has switched the connection.
    pub(super) fn start_tunnel(
        &self,
        response: &mut Response,
        upstream_response: http::Response<hyper::body::Incoming>,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        in_flight: gatel_state::InFlight,
        endpoint: &str,
    ) {
        let (parts, _) = upstream_response.into_parts();
        response.status_code(StatusCode::SWITCHING_PROTOCOLS);
        // A 101 carries only the framing headers that describe the switch.
        for name in [http::header::CONNECTION, http::header::UPGRADE] {
            if let Some(value) = parts.headers.get(&name) {
                response.headers_mut().insert(name, value.clone());
            }
        }
        for (name, value) in &parts.headers {
            let lower = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) {
                continue;
            }
            response.headers_mut().append(name.clone(), value.clone());
        }

        let mut extensions = parts.extensions;
        let Some(backend_upgrade) = extensions.remove::<hyper::upgrade::OnUpgrade>() else {
            tracing::warn!(
                target: "gatel::upstream",
                event = "upstream_upgrade_unavailable",
                upstream = self.upstream.spec.id.name(),
                endpoint,
            );
            return;
        };
        let Some(client_upgrade) = client_upgrade else {
            tracing::warn!(
                target: "gatel::connection",
                event = "client_upgrade_unavailable",
                detail = "the client connection cannot be upgraded",
            );
            return;
        };

        let timeouts = self.upstream.spec.timeouts;
        let upstream = self.upstream.spec.id.name().to_string();
        let endpoint = endpoint.to_string();
        tokio::spawn(async move {
            // Both futures resolve only after the 101 has actually been sent,
            // which is why this waits rather than upgrading in place.
            let (client, backend) = match tokio::join!(client_upgrade, backend_upgrade) {
                (Ok(client), Ok(backend)) => (client, backend),
                (client, backend) => {
                    tracing::debug!(
                        target: "gatel::connection",
                        event = "upgrade_failed",
                        client_error = client.err().map(|error| error.to_string()),
                        backend_error = backend.err().map(|error| error.to_string()),
                    );
                    return;
                }
            };
            let reason = upgrade::tunnel(
                client,
                backend,
                timeouts.upgrade_idle,
                timeouts.upgrade_duration,
                in_flight,
            )
            .await;
            tracing::debug!(
                target: "gatel::connection",
                event = "tunnel_closed",
                upstream,
                endpoint,
                reason = reason.as_str(),
            );
        });
    }
}
