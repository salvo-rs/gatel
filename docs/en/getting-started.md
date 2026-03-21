# Getting Started

This guide walks you through installing Gatel, writing your first configuration, and running the server.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Installation](#installation)
  - [Building from Source](#building-from-source)
  - [Optional Feature Flags](#optional-feature-flags)
- [CLI Commands](#cli-commands)
- [Your First Configuration](#your-first-configuration)
- [Running the Server](#running-the-server)
- [Validating Configuration](#validating-configuration)
- [Hot Reload](#hot-reload)
- [Graceful Shutdown](#graceful-shutdown)
- [Logging](#logging)
- [Admin API](#admin-api)
- [Next Steps](#next-steps)

---

## Prerequisites

- **Rust** 1.75 or later (2021 edition)
- **Cargo** (ships with Rust)
- A POSIX-compatible shell (Linux, macOS) or Windows with WSL/Git Bash

---

## Installation

### Building from Source

```bash
git clone https://github.com/salvo-rs/gatel.git
cd gatel
cargo build --release
```

The binary is produced at `target/release/gatel` (or `target\release\gatel.exe` on Windows).

### Optional Feature Flags

Gatel has two optional feature flags that are **disabled by default**:

| Flag | Purpose |
|---|---|
| `bcrypt` | Enables bcrypt password verification for `basic-auth` users. |
| `http3` | Enables the HTTP/3 (QUIC) listener via `quinn` and `h3`. |

To build with both:

```bash
cargo build --release --features "bcrypt,http3"
```

---

## CLI Commands

Gatel provides three subcommands:

### `run` -- Start the server

```bash
gatel run --config gatel.kdl
```

| Flag | Default | Description |
|---|---|---|
| `-c`, `--config` | `gatel.kdl` | Path to the KDL configuration file |

### `validate` -- Check configuration syntax

```bash
gatel validate --config gatel.kdl
```

Parses the configuration and reports any errors without starting the server. On success, prints the number of sites and routes:

```
Configuration is valid (2 site(s), 5 route(s))
```

On failure, prints the error and exits with code 1.

### `reload` -- Hot-reload the running server

```bash
gatel reload
```

On Unix, this sends `SIGHUP` to the running Gatel process. The server re-reads the config file, parses it, and atomically swaps in the new configuration. In-flight requests are not affected.

> **Note**: On Windows, `SIGHUP` is not available. Use the Admin API `POST /config/reload` endpoint instead.

---

## Your First Configuration

Create a file named `gatel.kdl`:

```kdl
global {
    admin ":2019"
    log level="info" format="pretty"
    http ":8080"
}

site "localhost" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}
```

This configuration:

1. Starts an HTTP listener on port 8080.
2. Starts an admin API on port 2019.
3. Defines a single site for `localhost` that forwards all requests to a backend on port 3000.

---

## Running the Server

Start the server:

```bash
gatel run --config gatel.kdl
```

You should see output like:

```
2025-01-15T10:30:00.000Z  INFO gatel: gatel starting
2025-01-15T10:30:00.001Z  INFO gatel: configuration loaded sites=1 http=0.0.0.0:8080
2025-01-15T10:30:00.002Z  INFO gatel_core::server: listening for HTTP connections http_addr=0.0.0.0:8080
2025-01-15T10:30:00.002Z  INFO gatel_core::admin: admin API server listening addr=0.0.0.0:2019
```

Test it:

```bash
curl http://localhost:8080/
```

---

## Validating Configuration

Before deploying a config change, validate it:

```bash
gatel validate --config gatel.kdl
```

This catches KDL syntax errors, missing required fields, unknown directives, and invalid values -- all without touching the running server.

---

## Hot Reload

Gatel supports zero-downtime configuration reloads. When a reload is triggered, Gatel:

1. Re-reads the configuration file from disk.
2. Parses and validates the new configuration.
3. Rebuilds the router from the new config.
4. Atomically swaps in the new config and router using `ArcSwap`.
5. Reloads TLS certificates if TLS is configured.

In-flight requests continue using the previous configuration snapshot. New requests use the new configuration immediately.

### Triggering a Reload

**Via SIGHUP (Unix only):**

```bash
kill -HUP $(pgrep gatel)
```

**Via the Admin API:**

```bash
curl -X POST http://localhost:2019/config/reload
```

Response on success:

```json
{"status": "reloaded"}
```

If the new config has errors, the reload is rejected and the server continues with the previous config:

```json
{"error": "config parse failed: missing required field: route handler"}
```

---

## Graceful Shutdown

When Gatel receives `SIGTERM`, `SIGINT`, or `Ctrl+C`, it begins a graceful shutdown:

1. **Stop accepting** new connections on all listeners.
2. **Drain** active connections -- wait for in-flight requests to complete.
3. **Force close** remaining connections after the grace period expires.
4. **Stop TLS maintenance** (ACME renewal loop).
5. **Exit**.

The grace period is configurable:

```kdl
global {
    grace-period "30s"
}
```

The default grace period is 30 seconds.

---

## Logging

Gatel uses the `tracing` crate for structured logging. Configure the log level and format in the `global` block:

```kdl
global {
    log level="info" format="pretty"
}
```

### Log Levels

| Level | Description |
|---|---|
| `error` | Only errors |
| `warn` | Errors and warnings |
| `info` | Normal operational messages (default) |
| `debug` | Detailed diagnostic information |
| `trace` | Very verbose, protocol-level details |

### Log Formats

| Format | Description |
|---|---|
| `pretty` | Human-readable, colored terminal output (default) |
| `json` | Structured JSON lines, suitable for log aggregation |

Example JSON log output:

```json
{"timestamp":"2025-01-15T10:30:00.123Z","level":"INFO","target":"gatel_core::middleware::logging","fields":{"client":"127.0.0.1:52340","method":"GET","path":"/api/users","status":200,"latency_ms":12},"message":"request handled"}
```

You can also override the log level at runtime using the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug gatel run --config gatel.kdl
```

---

## Admin API

When `admin` is configured, Gatel runs a lightweight REST API on a separate port:

```kdl
global {
    admin ":2019"
}
```

### Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Health check. Returns `{"status": "healthy"}`. |
| `GET` | `/config` | Returns the current configuration as JSON. |
| `POST` | `/config/reload` | Triggers a hot reload from the config file. |
| `GET` | `/upstreams` | Lists all configured upstream backends with their sites and routes. |
| `GET` | `/metrics` | Returns Prometheus-format metrics. |

### Examples

**Health check:**

```bash
curl http://localhost:2019/health
```

```json
{"status": "healthy"}
```

**View current config:**

```bash
curl http://localhost:2019/config
```

**List upstreams:**

```bash
curl http://localhost:2019/upstreams
```

```json
[
  {
    "site": "app.example.com",
    "route": "/api/*",
    "address": "127.0.0.1:3001",
    "weight": 3
  },
  {
    "site": "app.example.com",
    "route": "/api/*",
    "address": "127.0.0.1:3002",
    "weight": 1
  }
]
```

**Scrape Prometheus metrics:**

```bash
curl http://localhost:2019/metrics
```

```
# HELP gatel_requests_total Total number of HTTP requests.
# TYPE gatel_requests_total counter
gatel_requests_total{host="localhost",method="GET",status="200"} 1523

# HELP gatel_request_duration_seconds Total request processing time in seconds.
# TYPE gatel_request_duration_seconds histogram
gatel_request_duration_seconds_sum{host="localhost",method="GET"} 18.234100
gatel_request_duration_seconds_count{host="localhost",method="GET"} 1523

# HELP gatel_active_connections Current number of active connections.
# TYPE gatel_active_connections gauge
gatel_active_connections 7
```

---

## Next Steps

- [Configuration Reference](configuration.md) -- Learn every configuration directive.
- [Reverse Proxy](reverse-proxy.md) -- Set up load balancing, health checks, and retries.
- [Middleware](middleware.md) -- Add rate limiting, compression, authentication, and more.
- [TLS and ACME](tls-and-acme.md) -- Enable HTTPS with automatic certificates.
- [Advanced Features](advanced-features.md) -- HTTP/3, WebSocket, FastCGI, stream proxy, and more.
