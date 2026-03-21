# TLS and ACME

Gatel provides comprehensive TLS support including automatic certificate management via ACME, manual PEM certificates, mutual TLS (mTLS) client verification, and on-demand TLS for dynamic domains.

## Table of Contents

- [Overview](#overview)
- [Automatic Certificates (ACME)](#automatic-certificates-acme)
  - [Supported CAs](#supported-cas)
  - [Challenge Types](#challenge-types)
  - [Certificate Storage](#certificate-storage)
  - [Certificate Renewal](#certificate-renewal)
- [Manual Certificates](#manual-certificates)
- [Certificate Resolution Order](#certificate-resolution-order)
- [Mutual TLS (mTLS)](#mutual-tls-mtls)
  - [Required Mode](#required-mode)
  - [Optional Mode](#optional-mode)
- [On-Demand TLS](#on-demand-tls)
  - [Ask URL](#ask-url)
  - [Rate Limiting](#rate-limiting)
- [HTTPS Listener](#https-listener)
- [TLS Implementation Details](#tls-implementation-details)
- [Hot Reload](#hot-reload)
- [Examples](#examples)
  - [ACME with Let's Encrypt](#acme-with-lets-encrypt)
  - [Manual Certificates](#manual-certificates-example)
  - [Mixed ACME and Manual](#mixed-acme-and-manual)
  - [mTLS for Internal Services](#mtls-for-internal-services)
  - [On-Demand TLS for SaaS](#on-demand-tls-for-saas)

---

## Overview

Gatel's TLS system uses a **composite certificate resolver** that handles certificate selection at TLS handshake time:

1. **Manual certificates**: Per-site PEM files take highest priority. Looked up by SNI (Server Name Indication).
2. **ACME-managed certificates**: For sites without manual certs, certificates are automatically obtained and renewed via the ACME protocol (certon library).
3. **On-demand certificates**: For unknown domains, certificates can be obtained at handshake time if on-demand TLS is configured.

The TLS stack is built on:
- **rustls 0.23** for the TLS implementation (using the ring cryptographic backend).
- **tokio-rustls 0.26** for async TLS accept.
- **certon** (path dependency) for ACME certificate management.

---

## Automatic Certificates (ACME)

ACME (Automatic Certificate Management Environment) automates certificate issuance and renewal. Configure it in the global `tls` block:

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}
```

Any site that does **not** have a per-site `tls` block with manual cert/key paths is automatically enrolled in ACME management.

### Supported CAs

| Config Value | Certificate Authority | Notes |
|---|---|---|
| `letsencrypt` or `le` | Let's Encrypt (production) | Trusted certs; rate limits apply |
| `letsencrypt-staging` or `le-staging` | Let's Encrypt (staging) | Untrusted certs; higher rate limits; for testing |
| `zerossl` | ZeroSSL (production) | Alternative CA |

### Challenge Types

| Challenge | Config | Requirements |
|---|---|---|
| HTTP-01 | `challenge "http-01"` | Port 80 must be reachable from the internet |
| TLS-ALPN-01 | `challenge "tls-alpn-01"` | Port 443 must be reachable from the internet |

**HTTP-01** is the default and most common challenge type. Gatel uses a shared in-memory map for challenge tokens. The HTTP listener on port 80 serves challenge responses via the `AcmeChallengeMiddleware` -- no separate listener is needed.

**TLS-ALPN-01** performs the challenge during the TLS handshake itself. This is useful when port 80 is not available.

### Certificate Storage

Certificates are stored on disk by certon's `FileStorage` backend. The default storage location is in the system's data directory. Certificates persist across restarts.

### Certificate Renewal

The certon maintenance loop runs as a background Tokio task. It periodically checks certificate expiration and renews certificates before they expire. The maintenance loop is stopped during graceful shutdown.

---

## Manual Certificates

For sites where you have your own certificates (e.g., from an internal CA), specify PEM files per site:

```kdl
site "internal.example.com" {
    tls {
        cert "/etc/gatel/certs/internal.pem"
        key "/etc/gatel/certs/internal-key.pem"
    }
    route "/*" {
        proxy "127.0.0.1:8080"
    }
}
```

| Directive | Description |
|---|---|
| `cert` | Path to the certificate chain PEM file (fullchain: leaf + intermediates) |
| `key` | Path to the private key PEM file |

Manual certificates are loaded at startup (and on reload) using `certon::Certificate::from_pem_files` for robust PEM parsing.

**Sites with manual certificates are excluded from ACME management.** They take priority over any ACME-issued certificate for the same hostname.

---

## Certificate Resolution Order

When a TLS handshake arrives with an SNI hostname, Gatel resolves the certificate in this order:

1. **Manual certificates**: The `CompositeResolver` checks the per-site manual certificate map first. If a match is found, it is served immediately.
2. **ACME resolver**: If no manual certificate matches, the certon `CertResolver` is consulted. This serves certificates from the certificate cache.
3. **On-demand (if configured)**: If the certon resolver does not have a certificate and on-demand TLS is enabled, a new certificate is obtained in the background.

---

## Mutual TLS (mTLS)

Mutual TLS requires clients to present a certificate signed by a trusted CA. This is used for service-to-service authentication, zero-trust architectures, and high-security environments.

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
    client-auth required=true {
        ca-cert "/etc/gatel/client-ca.pem"
        ca-cert "/etc/gatel/partner-ca.pem"
    }
}
```

### Configuration

| Property | Type | Default | Description |
|---|---|---|---|
| `required` | boolean | `true` | Whether client certificates are mandatory |

| Directive | Description |
|---|---|
| `ca-cert` | Path to a PEM file with CA certificates used to verify client certs. Multiple `ca-cert` directives are allowed. |

### Required Mode

```kdl
client-auth required=true {
    ca-cert "/etc/gatel/client-ca.pem"
}
```

Clients **must** present a valid certificate signed by one of the configured CAs. Connections without a valid client certificate are rejected at the TLS handshake level.

### Optional Mode

```kdl
client-auth required=false {
    ca-cert "/etc/gatel/client-ca.pem"
}
```

Clients **may** present a certificate. If presented, it must be valid. If not presented, the connection proceeds without client authentication. This is useful for services that optionally elevate trust when a client cert is available.

### Implementation Details

- Uses rustls `WebPkiClientVerifier` for standards-compliant certificate chain validation.
- All CA certificates from all `ca-cert` files are loaded into a single `RootCertStore`.
- The client certificate verifier is rebuilt on configuration reload.

---

## On-Demand TLS

On-demand TLS obtains certificates automatically when a TLS handshake arrives for a hostname that does not yet have a certificate. This is designed for SaaS platforms and multi-tenant applications where the set of domains is not known in advance.

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
    on-demand ask="https://auth.example.com/check" rate-limit=10
}
```

### Ask URL

The `ask` URL is a safety mechanism. Before issuing a certificate for an unknown domain, Gatel makes a GET request to:

```
{ask}?domain={sni_hostname}
```

- **200 OK**: The domain is allowed; proceed with certificate issuance.
- **Any other status**: The domain is rejected; the TLS handshake fails.

This prevents abuse (e.g., an attacker pointing thousands of domains at your server to exhaust CA rate limits).

If no `ask` URL is configured, on-demand TLS only issues certificates for hostnames that appear in the site configuration (the host allowlist).

### Rate Limiting

The `rate-limit` property limits how many certificates can be issued per minute. This is a second layer of protection against abuse.

```kdl
on-demand ask="https://auth.example.com/check" rate-limit=5
```

This allows at most 5 new certificate issuances per minute across all domains.

### How It Works

1. A TLS handshake arrives with an SNI hostname that has no cached certificate.
2. The `CompositeResolver` falls through to the certon on-demand resolver.
3. The decision function runs:
   - If an `ask` URL is configured, it is queried synchronously (using `tokio::task::block_in_place`).
   - If a host allowlist exists (derived from configured site hostnames), the domain must be in the list.
   - If a rate limiter is configured, it must have available capacity.
4. If allowed, the ACME issuer obtains a certificate in the background.
5. The certificate is cached and served on subsequent handshakes.

---

## HTTPS Listener

When a `tls` block is present, Gatel starts an HTTPS listener in addition to the HTTP listener:

```kdl
global {
    http ":80"
    https ":443"
}

tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
}
```

- The HTTP listener (port 80) continues to serve HTTP requests and ACME HTTP-01 challenge responses.
- The HTTPS listener (port 443) performs TLS handshakes using the composite resolver and serves HTTPS requests.
- Both listeners share the same routing and middleware infrastructure.
- A fresh `TlsAcceptor` is obtained for each new connection, so hot-reloaded TLS configs take effect immediately for new connections.

---

## TLS Implementation Details

### Protocol Versions

Gatel supports TLS 1.2 and TLS 1.3 (configured via `rustls::ServerConfig::builder_with_provider` with safe default protocol versions).

### Cryptographic Backend

The `ring` cryptographic library is used for all TLS operations (via the `rustls` ring provider).

### ALPN Negotiation

- HTTP/1.1 and HTTP/2 are negotiated automatically by `hyper-util`.
- HTTP/3 requires a separate QUIC listener with `h3` in the ALPN list.

### ServerConfig Construction

The rustls `ServerConfig` is built with:
1. The composite certificate resolver (manual + ACME).
2. Optional client certificate verifier (for mTLS).
3. The config is stored behind an `ArcSwap` for lock-free hot reload.

---

## Hot Reload

When a configuration reload is triggered (via SIGHUP or the admin API), the TLS configuration is reloaded:

1. **Manual certificates**: All per-site PEM files are re-read from disk. If any file fails to load, the reload is aborted and the old config remains active.
2. **ACME domains**: Newly added sites are enrolled in ACME management. Removed sites stop being managed.
3. **mTLS verifier**: The client certificate verifier is rebuilt with the updated CA certificates.
4. **ServerConfig swap**: A new `rustls::ServerConfig` is built and atomically swapped in via `ArcSwap`.

In-flight connections are not affected -- they continue using the `ServerConfig` snapshot from when they were accepted.

---

## Examples

### ACME with Let's Encrypt

Automatic certificates for all sites:

```kdl
global {
    http ":80"
    https ":443"
}

tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

site "app.example.com" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}

site "api.example.com" {
    route "/*" {
        proxy "127.0.0.1:4000"
    }
}
```

Both `app.example.com` and `api.example.com` get automatic certificates.

### Manual Certificates Example

No ACME -- use your own certificates:

```kdl
global {
    https ":443"
}

tls {}

site "secure.example.com" {
    tls {
        cert "/etc/ssl/certs/secure.example.com.pem"
        key "/etc/ssl/private/secure.example.com.key"
    }
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}
```

### Mixed ACME and Manual

Some sites use ACME, others use manual certs:

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
}

// This site uses ACME (no per-site tls block)
site "public.example.com" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}

// This site uses a manual certificate
site "internal.example.com" {
    tls {
        cert "/etc/gatel/internal.pem"
        key "/etc/gatel/internal.key"
    }
    route "/*" {
        proxy "127.0.0.1:4000"
    }
}
```

### mTLS for Internal Services

Require client certificates from a corporate CA:

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
    client-auth required=true {
        ca-cert "/etc/gatel/corp-root-ca.pem"
        ca-cert "/etc/gatel/corp-intermediate-ca.pem"
    }
}

site "api.internal.example.com" {
    route "/*" {
        proxy "127.0.0.1:8080"
    }
}
```

All clients must present a certificate signed by either the root or intermediate corporate CA.

### On-Demand TLS for SaaS

A multi-tenant SaaS platform where customers bring their own domains:

```kdl
tls {
    acme {
        email "admin@saas.example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
    on-demand ask="https://api.saas.example.com/domains/check" rate-limit=10
}

site "*" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}
```

When a customer's domain (e.g., `shop.customer.com`) points to this server:
1. The TLS handshake triggers on-demand certificate issuance.
2. Gatel checks `https://api.saas.example.com/domains/check?domain=shop.customer.com` to verify the domain is authorized.
3. A Let's Encrypt certificate is obtained (rate-limited to 10/minute).
4. The certificate is cached for subsequent connections.
5. The wildcard `*` site catches all hostnames and proxies to the application.
