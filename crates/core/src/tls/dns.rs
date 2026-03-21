//! DNS provider implementations for ACME DNS-01 challenges.
//!
//! Each provider implements [`certon::DnsProvider`], which requires two async
//! methods:
//! - `set_record(zone, name, value, ttl)` — create a TXT record
//! - `delete_record(zone, name, value)` — remove a TXT record
//!
//! Providers are constructed from a [`DnsProviderConfig`] and use `reqwest`
//! for HTTP API calls (the same client that is already a workspace dependency).

use async_trait::async_trait;
use tracing::debug;

use crate::ProxyError;
use crate::config::DnsProviderConfig;

// ---------------------------------------------------------------------------
// Shared HTTP client helper
// ---------------------------------------------------------------------------

/// Build a `reqwest::Client` suitable for DNS API calls (30-second timeout).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// Error conversion helpers
// ---------------------------------------------------------------------------

/// Convert a reqwest error to a certon `Other` error.
fn req_err(e: reqwest::Error) -> certon::Error {
    certon::Error::Other(e.to_string())
}

/// Convert a serde_json error to a certon `Other` error.
fn json_err(e: serde_json::Error) -> certon::Error {
    certon::Error::Other(e.to_string())
}

// ---------------------------------------------------------------------------
// Cloudflare
// ---------------------------------------------------------------------------

/// DNS provider backed by the Cloudflare API (api.cloudflare.com/client/v4).
///
/// Required config field: `api-token`.
pub struct CloudflareDns {
    api_token: String,
    client: reqwest::Client,
}

impl CloudflareDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_token = cfg
            .api_token
            .clone()
            .ok_or_else(|| ProxyError::Internal("cloudflare requires api-token".into()))?;
        Ok(Self {
            api_token,
            client: http_client(),
        })
    }

    async fn get_zone_id(&self, zone: &str) -> certon::Result<String> {
        let zone_name = zone.trim_end_matches('.');
        let resp = self
            .client
            .get("https://api.cloudflare.com/client/v4/zones")
            .bearer_auth(&self.api_token)
            .query(&[("name", zone_name)])
            .send()
            .await
            .map_err(req_err)?;
        let body: serde_json::Value = resp.json().await.map_err(req_err)?;
        body["result"][0]["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| certon::Error::Other(format!("cloudflare zone not found: {zone_name}")))
    }
}

#[async_trait]
impl certon::DnsProvider for CloudflareDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let zone_id = self.get_zone_id(zone).await?;
        let fqdn = format!("{}.{}", name, zone.trim_end_matches('.'));
        let body = serde_json::json!({
            "type": "TXT",
            "name": fqdn,
            "content": value,
            "ttl": ttl,
        });
        self.client
            .post(format!(
                "https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records"
            ))
            .bearer_auth(&self.api_token)
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "cloudflare", record = %fqdn, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let zone_id = self.get_zone_id(zone).await?;
        let fqdn = format!("{}.{}", name, zone.trim_end_matches('.'));
        let resp = self
            .client
            .get(format!(
                "https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records"
            ))
            .bearer_auth(&self.api_token)
            .query(&[("type", "TXT"), ("name", fqdn.as_str()), ("content", value)])
            .send()
            .await
            .map_err(req_err)?;
        let body: serde_json::Value = resp.json().await.map_err(req_err)?;
        if let Some(records) = body["result"].as_array() {
            for record in records {
                if let Some(id) = record["id"].as_str() {
                    self.client
                        .delete(format!(
                            "https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records/{id}"
                        ))
                        .bearer_auth(&self.api_token)
                        .send()
                        .await
                        .map_err(req_err)?;
                    debug!(provider = "cloudflare", record = %fqdn, "DNS TXT record deleted");
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DigitalOcean
// ---------------------------------------------------------------------------

/// DNS provider backed by the DigitalOcean API (api.digitalocean.com/v2).
///
/// Required config field: `api-token`.
pub struct DigitalOceanDns {
    api_token: String,
    client: reqwest::Client,
}

impl DigitalOceanDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_token = cfg
            .api_token
            .clone()
            .ok_or_else(|| ProxyError::Internal("digitalocean requires api-token".into()))?;
        Ok(Self {
            api_token,
            client: http_client(),
        })
    }
}

#[async_trait]
impl certon::DnsProvider for DigitalOceanDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "data": value,
            "ttl": ttl,
        });
        self.client
            .post(format!(
                "https://api.digitalocean.com/v2/domains/{domain}/records"
            ))
            .bearer_auth(&self.api_token)
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "digitalocean", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        let resp = self
            .client
            .get(format!(
                "https://api.digitalocean.com/v2/domains/{domain}/records"
            ))
            .bearer_auth(&self.api_token)
            .query(&[("type", "TXT"), ("name", name)])
            .send()
            .await
            .map_err(req_err)?;
        let body: serde_json::Value = resp.json().await.map_err(req_err)?;
        if let Some(records) = body["domain_records"].as_array() {
            for record in records {
                if record["data"].as_str() == Some(value) {
                    if let Some(id) = record["id"].as_u64() {
                        self.client
                            .delete(format!(
                                "https://api.digitalocean.com/v2/domains/{domain}/records/{id}"
                            ))
                            .bearer_auth(&self.api_token)
                            .send()
                            .await
                            .map_err(req_err)?;
                        debug!(provider = "digitalocean", record = %name, "DNS TXT record deleted");
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Route53
// ---------------------------------------------------------------------------

/// DNS provider backed by AWS Route 53.
///
/// This implementation shells out to the `aws` CLI, which must be installed
/// and configured with appropriate credentials (via environment variables
/// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, or an IAM
/// instance/task role).
///
/// Required config field: `options.hosted-zone-id` — the Route 53 Hosted Zone
/// ID (e.g. `Z1234ABCDEF`).
pub struct Route53Dns {
    hosted_zone_id: String,
}

impl Route53Dns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let hosted_zone_id = cfg.options.get("hosted-zone-id").cloned().ok_or_else(|| {
            ProxyError::Internal("route53 requires options.hosted-zone-id".into())
        })?;
        Ok(Self { hosted_zone_id })
    }

    async fn change_record(
        &self,
        action: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let fqdn = if name.ends_with('.') {
            name.to_string()
        } else {
            format!("{name}.")
        };
        let batch = serde_json::json!({
            "Changes": [{
                "Action": action,
                "ResourceRecordSet": {
                    "Name": fqdn,
                    "Type": "TXT",
                    "TTL": ttl,
                    "ResourceRecords": [{"Value": format!("\"{}\"", value)}]
                }
            }]
        });
        let batch_str = serde_json::to_string(&batch).map_err(json_err)?;

        let output = tokio::process::Command::new("aws")
            .args([
                "route53",
                "change-resource-record-sets",
                "--hosted-zone-id",
                &self.hosted_zone_id,
                "--change-batch",
                &batch_str,
            ])
            .output()
            .await
            .map_err(|e| {
                certon::Error::Other(format!("failed to invoke aws CLI for route53: {e}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(certon::Error::Other(format!(
                "aws route53 change-resource-record-sets failed: {stderr}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl certon::DnsProvider for Route53Dns {
    async fn set_record(
        &self,
        _zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        self.change_record("UPSERT", name, value, ttl).await?;
        debug!(provider = "route53", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, _zone: &str, name: &str, value: &str) -> certon::Result<()> {
        // DELETE requires the TTL to match; use 300 as a safe default.
        self.change_record("DELETE", name, value, 300).await?;
        debug!(provider = "route53", record = %name, "DNS TXT record deleted");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DNSimple
// ---------------------------------------------------------------------------

/// DNS provider backed by the DNSimple API (api.dnsimple.com/v2).
///
/// Required config fields: `api-token`, `options.account-id`.
pub struct DnSimpleDns {
    api_token: String,
    account_id: String,
    client: reqwest::Client,
}

impl DnSimpleDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_token = cfg
            .api_token
            .clone()
            .ok_or_else(|| ProxyError::Internal("dnsimple requires api-token".into()))?;
        let account_id =
            cfg.options.get("account-id").cloned().ok_or_else(|| {
                ProxyError::Internal("dnsimple requires options.account-id".into())
            })?;
        Ok(Self {
            api_token,
            account_id,
            client: http_client(),
        })
    }
}

#[async_trait]
impl certon::DnsProvider for DnSimpleDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let zone_name = zone.trim_end_matches('.');
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": value,
            "ttl": ttl,
        });
        self.client
            .post(format!(
                "https://api.dnsimple.com/v2/{}/zones/{zone_name}/records",
                self.account_id
            ))
            .bearer_auth(&self.api_token)
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "dnsimple", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let zone_name = zone.trim_end_matches('.');
        let resp = self
            .client
            .get(format!(
                "https://api.dnsimple.com/v2/{}/zones/{zone_name}/records",
                self.account_id
            ))
            .bearer_auth(&self.api_token)
            .query(&[("type", "TXT"), ("name", name)])
            .send()
            .await
            .map_err(req_err)?;
        let body: serde_json::Value = resp.json().await.map_err(req_err)?;
        if let Some(records) = body["data"].as_array() {
            for record in records {
                if record["content"].as_str() == Some(value) {
                    if let Some(id) = record["id"].as_u64() {
                        self.client
                            .delete(format!(
                                "https://api.dnsimple.com/v2/{}/zones/{zone_name}/records/{id}",
                                self.account_id
                            ))
                            .bearer_auth(&self.api_token)
                            .send()
                            .await
                            .map_err(req_err)?;
                        debug!(provider = "dnsimple", record = %name, "DNS TXT record deleted");
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Porkbun
// ---------------------------------------------------------------------------

/// DNS provider backed by the Porkbun API (api.porkbun.com/api/json/v3).
///
/// Required config fields: `api-key`, `api-secret`.
pub struct PorkbunDns {
    api_key: String,
    api_secret: String,
    client: reqwest::Client,
}

impl PorkbunDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_key = cfg
            .api_key
            .clone()
            .ok_or_else(|| ProxyError::Internal("porkbun requires api-key".into()))?;
        let api_secret = cfg
            .api_secret
            .clone()
            .ok_or_else(|| ProxyError::Internal("porkbun requires api-secret".into()))?;
        Ok(Self {
            api_key,
            api_secret,
            client: http_client(),
        })
    }

    fn auth_body(&self) -> serde_json::Value {
        serde_json::json!({
            "apikey": self.api_key,
            "secretapikey": self.api_secret,
        })
    }
}

#[async_trait]
impl certon::DnsProvider for PorkbunDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        let mut body = self.auth_body();
        body["type"] = serde_json::Value::String("TXT".into());
        body["name"] = serde_json::Value::String(name.to_string());
        body["content"] = serde_json::Value::String(value.to_string());
        body["ttl"] = serde_json::Value::String(ttl.to_string());
        self.client
            .post(format!(
                "https://api.porkbun.com/api/json/v3/dns/create/{domain}"
            ))
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "porkbun", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        let body = self.auth_body();
        let resp = self
            .client
            .post(format!(
                "https://api.porkbun.com/api/json/v3/dns/retrieveByNameType/{domain}/TXT/{name}"
            ))
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        let resp_body: serde_json::Value = resp.json().await.map_err(req_err)?;
        if let Some(records) = resp_body["records"].as_array() {
            for record in records {
                if record["content"].as_str() == Some(value) {
                    if let Some(id) = record["id"].as_str() {
                        let del_body = self.auth_body();
                        self.client
                            .post(format!(
                                "https://api.porkbun.com/api/json/v3/dns/delete/{domain}/{id}"
                            ))
                            .json(&del_body)
                            .send()
                            .await
                            .map_err(req_err)?;
                        debug!(provider = "porkbun", record = %name, "DNS TXT record deleted");
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OVH
// ---------------------------------------------------------------------------

/// DNS provider backed by the OVH API (eu.api.ovh.com/1.0).
///
/// Required config fields: `api-key` (application key), `api-secret`
/// (application secret), `options.consumer-key`.
///
/// OVH uses a time-based HMAC-SHA1 signature. This implementation computes the
/// signature using a pure-Rust SHA-1 over the pre-hash string that already
/// embeds the application secret (matching OVH's documented scheme).
pub struct OvhDns {
    app_key: String,
    app_secret: String,
    consumer_key: String,
    client: reqwest::Client,
}

impl OvhDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let app_key = cfg
            .api_key
            .clone()
            .ok_or_else(|| ProxyError::Internal("ovh requires api-key (application key)".into()))?;
        let app_secret = cfg.api_secret.clone().ok_or_else(|| {
            ProxyError::Internal("ovh requires api-secret (application secret)".into())
        })?;
        let consumer_key = cfg
            .options
            .get("consumer-key")
            .cloned()
            .ok_or_else(|| ProxyError::Internal("ovh requires options.consumer-key".into()))?;
        Ok(Self {
            app_key,
            app_secret,
            consumer_key,
            client: http_client(),
        })
    }

    /// Compute the OVH request signature using a minimal SHA-1 implementation.
    ///
    /// OVH signature format: `"$1$" + hex(SHA1(pre_hash))` where
    /// `pre_hash = app_secret + "+" + consumer_key + "+" + method + "+" + url + "+" + body + "+" +
    /// timestamp`.
    fn sign(&self, method: &str, url: &str, body: &str, timestamp: u64) -> String {
        let pre_hash = format!(
            "{}+{}+{}+{}+{}+{}",
            self.app_secret, self.consumer_key, method, url, body, timestamp
        );
        let hex = sha1_hex(pre_hash.as_bytes());
        format!("$1${hex}")
    }
}

/// Minimal SHA-1 implementation (no external crate required).
///
/// Based on RFC 3174 / FIPS PUB 180-4. Used only for the OVH API signature
/// which mandates SHA-1. This avoids adding the `ring` crate as a direct
/// dependency of `gatel-core`.
fn sha1_hex(data: &[u8]) -> String {
    // SHA-1 constants
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    // Pre-processing: append bit '1' then zeros then length (big-endian 64-bit)
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) chunk
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    h.iter()
        .map(|v| format!("{v:08x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl certon::DnsProvider for OvhDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let zone_name = zone.trim_end_matches('.');
        let url = format!("https://eu.api.ovh.com/1.0/domain/zone/{zone_name}/record");
        let body = serde_json::json!({
            "fieldType": "TXT",
            "subDomain": name,
            "target": value,
            "ttl": ttl,
        });
        let body_str = serde_json::to_string(&body).map_err(json_err)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sig = self.sign("POST", &url, &body_str, timestamp);
        self.client
            .post(&url)
            .header("X-Ovh-Application", &self.app_key)
            .header("X-Ovh-Consumer", &self.consumer_key)
            .header("X-Ovh-Timestamp", timestamp.to_string())
            .header("X-Ovh-Signature", sig)
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "ovh", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let zone_name = zone.trim_end_matches('.');
        let list_url = format!(
            "https://eu.api.ovh.com/1.0/domain/zone/{zone_name}/record?fieldType=TXT&subDomain={name}"
        );
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sig = self.sign("GET", &list_url, "", timestamp);
        let resp = self
            .client
            .get(&list_url)
            .header("X-Ovh-Application", &self.app_key)
            .header("X-Ovh-Consumer", &self.consumer_key)
            .header("X-Ovh-Timestamp", timestamp.to_string())
            .header("X-Ovh-Signature", sig)
            .send()
            .await
            .map_err(req_err)?;
        let ids: Vec<u64> = resp.json().await.map_err(req_err)?;
        for id in ids {
            let detail_url =
                format!("https://eu.api.ovh.com/1.0/domain/zone/{zone_name}/record/{id}");
            let ts2 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let sig2 = self.sign("GET", &detail_url, "", ts2);
            let detail_resp = self
                .client
                .get(&detail_url)
                .header("X-Ovh-Application", &self.app_key)
                .header("X-Ovh-Consumer", &self.consumer_key)
                .header("X-Ovh-Timestamp", ts2.to_string())
                .header("X-Ovh-Signature", sig2)
                .send()
                .await
                .map_err(req_err)?;
            let detail: serde_json::Value = detail_resp.json().await.map_err(req_err)?;
            if detail["target"].as_str() == Some(value) {
                let del_url =
                    format!("https://eu.api.ovh.com/1.0/domain/zone/{zone_name}/record/{id}");
                let ts3 = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let sig3 = self.sign("DELETE", &del_url, "", ts3);
                self.client
                    .delete(&del_url)
                    .header("X-Ovh-Application", &self.app_key)
                    .header("X-Ovh-Consumer", &self.consumer_key)
                    .header("X-Ovh-Timestamp", ts3.to_string())
                    .header("X-Ovh-Signature", sig3)
                    .send()
                    .await
                    .map_err(req_err)?;
                debug!(provider = "ovh", record = %name, "DNS TXT record deleted");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// deSEC
// ---------------------------------------------------------------------------

/// DNS provider backed by the deSEC API (desec.io/api/v1).
///
/// Required config field: `api-token`.
pub struct DesecDns {
    api_token: String,
    client: reqwest::Client,
}

impl DesecDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_token = cfg
            .api_token
            .clone()
            .ok_or_else(|| ProxyError::Internal("desec requires api-token".into()))?;
        Ok(Self {
            api_token,
            client: http_client(),
        })
    }
}

#[async_trait]
impl certon::DnsProvider for DesecDns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        // deSEC uses PATCH on rrsets; records must be quoted TXT values.
        let body = serde_json::json!([{
            "subname": name,
            "type": "TXT",
            "ttl": ttl,
            "records": [format!("\"{}\"", value)],
        }]);
        self.client
            .patch(format!("https://desec.io/api/v1/domains/{domain}/rrsets/"))
            .header("Authorization", format!("Token {}", self.api_token))
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "desec", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, _value: &str) -> certon::Result<()> {
        let domain = zone.trim_end_matches('.');
        // Delete the entire RRset for this subname (deSEC does not support
        // removing individual records from an rrset via the public API).
        self.client
            .delete(format!(
                "https://desec.io/api/v1/domains/{domain}/rrsets/{name}/TXT/"
            ))
            .header("Authorization", format!("Token {}", self.api_token))
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "desec", record = %name, "DNS TXT record deleted");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bunny DNS
// ---------------------------------------------------------------------------

/// DNS provider backed by the Bunny.net DNS API (api.bunny.net).
///
/// Required config fields: `api-token` (Bunny API access key),
/// `options.zone-id` (numeric DNS zone ID visible in the Bunny dashboard).
pub struct BunnyDns {
    api_token: String,
    zone_id: String,
    client: reqwest::Client,
}

impl BunnyDns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        let api_token = cfg
            .api_token
            .clone()
            .ok_or_else(|| ProxyError::Internal("bunny requires api-token".into()))?;
        let zone_id = cfg
            .options
            .get("zone-id")
            .cloned()
            .ok_or_else(|| ProxyError::Internal("bunny requires options.zone-id".into()))?;
        Ok(Self {
            api_token,
            zone_id,
            client: http_client(),
        })
    }
}

#[async_trait]
impl certon::DnsProvider for BunnyDns {
    async fn set_record(
        &self,
        _zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let body = serde_json::json!({
            "Type": 3,        // TXT record type in Bunny's enum
            "Name": name,
            "Value": value,
            "Ttl": ttl,
        });
        self.client
            .post(format!(
                "https://api.bunny.net/dnszone/{}/records",
                self.zone_id
            ))
            .header("AccessKey", &self.api_token)
            .json(&body)
            .send()
            .await
            .map_err(req_err)?;
        debug!(provider = "bunny", record = %name, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, _zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let resp = self
            .client
            .get(format!("https://api.bunny.net/dnszone/{}", self.zone_id))
            .header("AccessKey", &self.api_token)
            .send()
            .await
            .map_err(req_err)?;
        let body: serde_json::Value = resp.json().await.map_err(req_err)?;
        if let Some(records) = body["Records"].as_array() {
            for record in records {
                if record["Type"].as_u64() == Some(3)
                    && record["Name"].as_str() == Some(name)
                    && record["Value"].as_str() == Some(value)
                {
                    if let Some(id) = record["Id"].as_u64() {
                        self.client
                            .delete(format!(
                                "https://api.bunny.net/dnszone/{}/records/{id}",
                                self.zone_id
                            ))
                            .header("AccessKey", &self.api_token)
                            .send()
                            .await
                            .map_err(req_err)?;
                        debug!(provider = "bunny", record = %name, "DNS TXT record deleted");
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RFC 2136 (nsupdate)
// ---------------------------------------------------------------------------

/// DNS provider that uses RFC 2136 dynamic DNS updates via the `nsupdate`
/// command-line tool.
///
/// `nsupdate` must be installed on the host (part of BIND9 utils on most
/// distros). Optional config fields (all passed as `options`):
/// - `server`        — DNS server to send updates to (default: system resolver)
/// - `key-name`      — TSIG key name for authenticated updates
/// - `key-secret`    — TSIG key secret (base64-encoded)
/// - `key-algorithm` — TSIG algorithm (default: `hmac-sha256`)
pub struct Rfc2136Dns {
    server: Option<String>,
    key_name: Option<String>,
    key_secret: Option<String>,
    key_algorithm: String,
}

impl Rfc2136Dns {
    pub fn new(cfg: &DnsProviderConfig) -> Result<Self, ProxyError> {
        Ok(Self {
            server: cfg.options.get("server").cloned(),
            key_name: cfg.options.get("key-name").cloned(),
            key_secret: cfg.options.get("key-secret").cloned(),
            key_algorithm: cfg
                .options
                .get("key-algorithm")
                .cloned()
                .unwrap_or_else(|| "hmac-sha256".to_string()),
        })
    }

    async fn run_nsupdate(&self, commands: &str) -> certon::Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut cmd = tokio::process::Command::new("nsupdate");
        if self.key_name.is_some() {
            let key_name = self.key_name.as_deref().unwrap_or("");
            let key_secret = self.key_secret.as_deref().unwrap_or("");
            cmd.arg("-y");
            cmd.arg(format!(
                "{}:{}:{}",
                self.key_algorithm, key_name, key_secret
            ));
        }

        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| certon::Error::Other(format!("failed to spawn nsupdate: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(commands.as_bytes())
                .await
                .map_err(|e| certon::Error::Other(e.to_string()))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| certon::Error::Other(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(certon::Error::Other(format!("nsupdate failed: {stderr}")));
        }
        Ok(())
    }
}

#[async_trait]
impl certon::DnsProvider for Rfc2136Dns {
    async fn set_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
        ttl: u32,
    ) -> certon::Result<()> {
        let fqdn = format!("{}.{}.", name, zone.trim_end_matches('.'));
        let zone_fqdn = format!("{}.", zone.trim_end_matches('.'));
        let mut cmds = String::new();
        if let Some(ref server) = self.server {
            cmds.push_str(&format!("server {server}\n"));
        }
        cmds.push_str(&format!("zone {zone_fqdn}\n"));
        cmds.push_str(&format!("update add {fqdn} {ttl} TXT \"{value}\"\nsend\n"));
        self.run_nsupdate(&cmds).await?;
        debug!(provider = "rfc2136", record = %fqdn, "DNS TXT record created");
        Ok(())
    }

    async fn delete_record(&self, zone: &str, name: &str, value: &str) -> certon::Result<()> {
        let fqdn = format!("{}.{}.", name, zone.trim_end_matches('.'));
        let zone_fqdn = format!("{}.", zone.trim_end_matches('.'));
        let mut cmds = String::new();
        if let Some(ref server) = self.server {
            cmds.push_str(&format!("server {server}\n"));
        }
        cmds.push_str(&format!("zone {zone_fqdn}\n"));
        cmds.push_str(&format!("update delete {fqdn} TXT \"{value}\"\nsend\n"));
        self.run_nsupdate(&cmds).await?;
        debug!(provider = "rfc2136", record = %fqdn, "DNS TXT record deleted");
        Ok(())
    }
}
