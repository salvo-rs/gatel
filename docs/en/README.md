# Gatel Documentation

**Gatel** is a high-performance, Caddy-like reverse proxy built in Rust. It combines the ergonomics of a KDL configuration file with the performance of hyper 1.x, tokio-rustls, and the certon ACME client.

## At a Glance

| Feature | Details |
|---|---|
| Language | Rust (async, tokio 1) |
| HTTP engine | hyper 1, hyper-util |
| TLS | tokio-rustls 0.26, rustls 0.23 |
| ACME | certon (Let's Encrypt, ZeroSSL) |
| Config format | KDL 6 |
| Hot reload | SIGHUP / Admin API, lock-free via ArcSwap |
| Protocols | HTTP/1.1, HTTP/2, HTTP/3 (QUIC, optional), WebSocket, FastCGI, TCP stream |

---

## Table of Contents

- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Feature Highlights](#feature-highlights)
- [Documentation Map](#documentation-map)
- [Project Structure](#project-structure)
- [License](#license)

---

## Quick Start

```bash
# Build from source
cargo build --release

# Validate your configuration
gatel validate --config gatel.kdl

# Start the server
gatel run --config gatel.kdl
```

A minimal configuration file:

```kdl
site "localhost" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}
```

This listens on port 80 and forwards all traffic to a backend on port 3000.

---

## Architecture

```
Client
  |
  v
TCP Accept
  |
  v
[PROXY Protocol v1/v2]  (optional, if enabled)
  |
  v
TLS Handshake            (tokio-rustls + certon ACME)
  |
  v
HTTP Parse               (hyper auto-detect: HTTP/1.1, HTTP/2, HTTP/3)
  |
  v
Router                   (Host -> Site, Path + Matchers -> Route)
  |
  v
Middleware Chain          [logging -> ip_filter -> rate_limit -> auth ->
                           rewrite -> headers -> compress -> cache -> templates]
  |
  v
Terminal Handler         (ReverseProxy | FastCGI | FileServer | Redirect | Respond)
  |
  v
(if proxy) LoadBalancer -> select Backend -> hyper-util Client -> Upstream
```

### Key Design Decisions

- **Lock-free hot reload**: Configuration and the compiled router are stored behind `ArcSwap`. A reload atomically swaps the pointer; in-flight requests continue using the previous snapshot.
- **Middleware chain pattern**: Each route compiles into a `MiddlewareChain` with a stack of `Arc<dyn Middleware>` and a terminal `Arc<dyn Handler>`. The chain is traversed as a recursive async call stack.
- **Connection tracking**: Active connections are tracked with atomic counters and RAII guards (`ConnectionGuard`), enabling graceful shutdown with a configurable drain period.
- **Composite TLS resolver**: A `CompositeResolver` checks per-site manual PEM certificates first, then falls back to certon's ACME-backed resolver.

---

## Feature Highlights

### Reverse Proxy
Nine load-balancing strategies (round robin, weighted round robin, IP hash, least connections, URI hash, header hash, cookie hash, random, first), active and passive health checks, configurable retries, and header manipulation. See [Reverse Proxy](reverse-proxy.md).

### Automatic TLS
Automatic certificate issuance and renewal via Let's Encrypt or ZeroSSL using the HTTP-01 or TLS-ALPN-01 challenge. Supports manual PEM certificates per site, mTLS client verification, and on-demand TLS for dynamic domains. See [TLS and ACME](tls-and-acme.md).

### Middleware
Rate limiting (token bucket), response compression (gzip, zstd, brotli), IP filtering (CIDR allow/deny), basic authentication (plaintext or bcrypt), response caching (LRU with Cache-Control support), header manipulation with placeholders, URI rewriting, and server-side HTML templates. See [Middleware](middleware.md).

### Advanced Protocols
HTTP/3 (QUIC) via quinn, transparent WebSocket proxying, full FastCGI protocol implementation for PHP-FPM, L4 TCP stream proxy, and PROXY protocol v1/v2 support. See [Advanced Features](advanced-features.md).

### Static File Serving
MIME type detection, ETag and Last-Modified headers, conditional requests (304), byte-range requests, directory index files, and optional directory browsing with styled HTML listings.

### Admin API
REST endpoints for inspecting and managing the running server: read the current configuration, trigger a hot reload, check health, list upstream backends, and scrape Prometheus-format metrics.

### Observability
Structured logging via the `tracing` crate with pretty or JSON output. Prometheus metrics expose request counts, latency histograms, and active connection gauges.

---

## Documentation Map

| Document | Description |
|---|---|
| [Getting Started](getting-started.md) | Installation, first run, basic configuration |
| [Configuration Reference](configuration.md) | Complete KDL config reference for every directive |
| [Reverse Proxy](reverse-proxy.md) | Proxy, load balancing, health checks, retries, headers |
| [Middleware](middleware.md) | All middleware: rate limiting, compression, auth, caching, etc. |
| [TLS and ACME](tls-and-acme.md) | TLS, ACME auto-certs, mTLS, on-demand TLS |
| [Advanced Features](advanced-features.md) | HTTP/3, WebSocket, FastCGI, stream proxy, PROXY protocol, matchers, templates, caching |

---

## Project Structure

```
gatel/
  gatel/                   # Binary crate (CLI, signal handling, main)
    src/
      main.rs               # Entry point, tracing init, signal handler
      cli.rs                # Clap CLI definitions
  gatel-core/              # Library crate (all server logic)
    src/
      lib.rs                # Body types, error types
      config/
        mod.rs              # Re-exports
        types.rs            # AppConfig, GlobalConfig, TlsConfig, etc.
        parse.rs            # KDL parser
      server/
        mod.rs              # AppState, run(), accept loops
        http_server.rs      # HTTP/1+2 connection handler
        h3_server.rs        # HTTP/3 (QUIC) server (feature-gated)
        proxy_protocol.rs   # PROXY protocol v1/v2 parser
        graceful.rs         # Graceful shutdown coordinator
      router/
        mod.rs              # Host+path routing, compiled route dispatch
        matcher.rs          # Request matchers (method, header, query, IP, expression)
      proxy/
        mod.rs              # ReverseProxy handler, retry logic
        lb.rs               # 9 load-balancing strategies
        health.rs           # Active + passive health checkers
        upstream.rs         # UpstreamPool, Backend, ConnGuard
        websocket.rs        # WebSocket upgrade detection + tunnelling
        fastcgi.rs          # FastCGI protocol implementation
        dns_upstream.rs     # DNS-based dynamic upstream resolution
      middleware/
        mod.rs              # Middleware/Handler traits, chain builder
        logging.rs          # Access logging middleware
        compress.rs         # gzip/zstd/brotli compression
        headers.rs          # Request/response header manipulation
        rewrite.rs          # URI rewriting (strip prefix, template)
        redirect.rs         # HTTP redirect handler
        auth.rs             # Basic auth (plaintext + bcrypt)
        rate_limit.rs       # Per-IP token bucket rate limiter
        ip_filter.rs        # CIDR-based IP allow/deny
        cache.rs            # LRU response cache with Cache-Control
        templates.rs        # Server-side HTML template processing
        file_server.rs      # Static file serving with directory browsing
        metrics.rs          # Prometheus metrics collection
        acme_challenge.rs   # ACME HTTP-01 challenge responder
      admin/
        mod.rs              # Admin REST API server
      tls/
        mod.rs              # Re-exports
        manager.rs          # TlsManager, ACME setup, mTLS, on-demand TLS
      stream/
        mod.rs              # L4 TCP bidirectional stream proxy
  gatel.kdl                # Example configuration file
```

---

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` 1 | Async runtime |
| `hyper` 1 | HTTP/1.1 and HTTP/2 protocol |
| `hyper-util` 0.1 | Server and client utilities |
| `tokio-rustls` 0.26 | TLS acceptor |
| `rustls` 0.23 | TLS implementation (ring backend) |
| `certon` (path dep) | ACME certificate management |
| `kdl` 6 | KDL configuration parsing |
| `arc-swap` 1 | Lock-free atomic pointer swap for hot reload |
| `clap` 4 | CLI argument parsing |
| `tracing` + `tracing-subscriber` | Structured logging |
| `async-compression` 0.4 | gzip, zstd, brotli compression |
| `dashmap` 6 | Concurrent hash maps |
| `quinn` 0.11 | QUIC transport (optional, `http3` feature) |
| `h3` 0.0.8 | HTTP/3 protocol (optional, `http3` feature) |
| `bcrypt` 0.17 | Password hashing (optional, `bcrypt` feature) |

## Feature Flags

| Flag | Effect |
|---|---|
| `bcrypt` | Enables bcrypt password hashing for `basic-auth`. Without this flag, bcrypt hashes in the config are rejected at runtime. |
| `http3` | Enables the HTTP/3 (QUIC) listener via `quinn` and `h3`. Requires the `http3 true` directive in the global config block. |

Enable features during build:

```bash
cargo build --release --features "bcrypt,http3"
```
