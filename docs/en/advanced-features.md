# Advanced Features

This document covers Gatel's advanced capabilities: HTTP/3, WebSocket proxying, FastCGI, L4 stream proxy, PROXY protocol, request matchers, server-side templates, and response caching.

## Table of Contents

- [HTTP/3 (QUIC)](#http3-quic)
  - [Enabling HTTP/3](#enabling-http3)
  - [Alt-Svc Header](#alt-svc-header)
  - [How It Works](#how-http3-works)
- [WebSocket Proxying](#websocket-proxying)
  - [Detection](#websocket-detection)
  - [Proxying Mechanism](#websocket-proxying-mechanism)
- [FastCGI](#fastcgi)
  - [Configuration](#fastcgi-configuration)
  - [PHP-FPM Setup](#php-fpm-setup)
  - [CGI Environment Variables](#cgi-environment-variables)
  - [Path Splitting](#path-splitting)
- [Stream Proxy (L4 TCP)](#stream-proxy-l4-tcp)
  - [Configuration](#stream-proxy-configuration)
  - [Use Cases](#stream-proxy-use-cases)
- [PROXY Protocol](#proxy-protocol)
  - [Version 1 (Text)](#version-1-text)
  - [Version 2 (Binary)](#version-2-binary)
  - [Configuration](#proxy-protocol-configuration)
  - [How It Works](#how-proxy-protocol-works)
- [Request Matchers](#request-matchers)
  - [Method Matcher](#method-matcher)
  - [Path Matcher](#path-matcher)
  - [Header Matcher](#header-matcher)
  - [Query Matcher](#query-matcher)
  - [Remote IP Matcher](#remote-ip-matcher)
  - [Protocol Matcher](#protocol-matcher)
  - [Expression Matcher](#expression-matcher)
  - [Not Matcher](#not-matcher)
  - [Combining Matchers](#combining-matchers)
- [Server-Side Templates](#server-side-templates)
- [Response Caching](#response-caching)
- [Graceful Shutdown](#graceful-shutdown)
- [Hot Reload Internals](#hot-reload-internals)

---

## HTTP/3 (QUIC)

HTTP/3 is the latest version of the HTTP protocol, built on QUIC (UDP-based transport). It eliminates head-of-line blocking, provides faster connection establishment, and works better on lossy networks.

### Enabling HTTP/3

HTTP/3 requires two things:

1. **Compile-time**: Build with the `http3` feature flag.
2. **Run-time**: Enable it in the global config and configure TLS.

```bash
cargo build --release --features http3
```

```kdl
global {
    https ":443"
    http3 true
}

tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
    }
}
```

The HTTP/3 listener binds to the same address as the HTTPS listener (port 443 by default) but uses UDP.

### Alt-Svc Header

Gatel automatically adds an `Alt-Svc` header to HTTP/3 responses, advertising HTTP/3 availability:

```
Alt-Svc: h3=":443"; ma=2592000
```

This tells browsers and other HTTP/3-capable clients that they can upgrade to QUIC on subsequent requests. The `ma` (max-age) is set to 30 days.

### How HTTP/3 Works

1. A QUIC endpoint is created using `quinn` with a QUIC-compatible copy of the rustls `ServerConfig` (ALPN set to `h3`).
2. Incoming QUIC connections are accepted and handled by the `h3` crate.
3. Each HTTP/3 request is converted into an `http::Request<Body>` and routed through the same middleware and handler chain as HTTP/1+2 requests.
4. The response headers and body are written back over the QUIC stream.
5. Graceful shutdown closes the QUIC endpoint with a GOAWAY frame.

HTTP/3 connections participate in connection tracking and graceful shutdown, just like HTTP/1+2 connections.

---

## WebSocket Proxying

Gatel transparently proxies WebSocket connections. No special configuration is required.

### WebSocket Detection

A request is identified as a WebSocket upgrade when both conditions are true:

- The `Connection` header contains `Upgrade` (case-insensitive, may be comma-separated).
- The `Upgrade` header equals `websocket` (case-insensitive).

### WebSocket Proxying Mechanism

When a WebSocket upgrade is detected:

1. **Backend selection**: The load balancer selects a backend (same as regular HTTP).
2. **TCP connection**: A raw TCP connection is opened to the upstream backend.
3. **Upgrade request**: The HTTP upgrade request is forwarded to the upstream as raw HTTP/1.1 text.
4. **101 response**: The upstream's `101 Switching Protocols` response is read and validated.
5. **Client response**: A `101 Switching Protocols` response is sent to the client, including `Sec-WebSocket-Accept`, `Sec-WebSocket-Protocol`, and `Sec-WebSocket-Extensions` headers from the upstream.
6. **Bidirectional tunnel**: A `tokio::io::copy_bidirectional` task is spawned to relay bytes between the client and upstream until either side closes.

The WebSocket tunnel operates at the byte level -- Gatel does not interpret WebSocket frames. This means it works with any WebSocket sub-protocol.

WebSocket connections are counted as active connections for `least_conn` load balancing and graceful shutdown.

---

## FastCGI

Gatel includes a full FastCGI protocol implementation for proxying to PHP-FPM and other FastCGI servers.

### FastCGI Configuration

```kdl
site "php.example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/html"
            split ".php"
            index "index.php"
            env "APP_ENV" "production"
            env "DB_HOST" "localhost"
        }
    }
    route "/*" {
        root "/var/www/html"
        file-server
    }
}
```

| Directive | Default | Description |
|---|---|---|
| (positional) | -- | FastCGI server address (`host:port`) |
| `root` / `script-root` | `""` | Document root for script paths |
| `split` | -- | Path-info split marker (e.g., `.php`) |
| `index` | `"index.php"` | Index filename(s) for directory requests |
| `env` | -- | Extra environment variables (key-value pairs) |

### PHP-FPM Setup

To use Gatel with PHP-FPM:

1. Install PHP-FPM and configure it to listen on a TCP socket:

```ini
; /etc/php/8.2/fpm/pool.d/www.conf
listen = 127.0.0.1:9000
```

2. Configure Gatel to forward PHP requests:

```kdl
site "example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/html"
            split ".php"
            index "index.php"
        }
    }
    route "/*" {
        root "/var/www/html"
        file-server
    }
}
```

3. Place PHP files in `/var/www/html/` and access them through Gatel.

### CGI Environment Variables

Gatel sets the standard CGI environment variables for each FastCGI request:

| Variable | Source |
|---|---|
| `SCRIPT_FILENAME` | Document root + script path |
| `SCRIPT_NAME` | Script path portion of the URI |
| `PATH_INFO` | Path after the script (split by the `split` marker) |
| `QUERY_STRING` | URI query string |
| `REQUEST_METHOD` | HTTP method |
| `SERVER_NAME` | Host header |
| `SERVER_PORT` | Server port |
| `SERVER_PROTOCOL` | HTTP version |
| `CONTENT_TYPE` | Request Content-Type header |
| `CONTENT_LENGTH` | Request Content-Length header |
| `REMOTE_ADDR` | Client IP address |
| `REMOTE_PORT` | Client port |

Additional headers are passed as `HTTP_*` variables (e.g., `HTTP_ACCEPT`, `HTTP_COOKIE`).

Custom environment variables from the `env` directive are also included.

### Path Splitting

The `split` directive controls how the URI is divided into `SCRIPT_NAME` and `PATH_INFO`:

Given `split ".php"` and a request to `/app/index.php/api/users`:

- `SCRIPT_NAME` = `/app/index.php`
- `PATH_INFO` = `/api/users`
- `SCRIPT_FILENAME` = `/var/www/html/app/index.php`

---

## Stream Proxy (L4 TCP)

The stream proxy provides Layer 4 TCP proxying for non-HTTP protocols. It performs bidirectional byte copying between a client and an upstream server.

### Stream Proxy Configuration

```kdl
stream {
    listen ":3306" {
        proxy "mysql-primary:3306"
    }
    listen ":6379" {
        proxy "redis:6379"
    }
    listen ":5432" {
        proxy "postgres:5432"
    }
}
```

Each `listen` block:
- Binds a TCP listener on the specified address.
- Accepts incoming connections.
- Opens a TCP connection to the `proxy` target.
- Copies bytes bidirectionally using `tokio::io::copy_bidirectional`.

### Stream Proxy Use Cases

- **Database proxying**: MySQL, PostgreSQL, Redis, MongoDB.
- **Mail servers**: SMTP, IMAP.
- **Custom protocols**: Any TCP-based protocol.
- **Service mesh ingress**: Forward TCP traffic to internal services.

**Limitations:**
- No load balancing (single upstream per listener).
- No health checking.
- No TLS termination on the stream proxy (traffic is passed through as-is).

---

## PROXY Protocol

The PROXY protocol allows upstream load balancers, CDNs, and proxies to pass the real client IP address to Gatel by prepending a protocol header to the TCP connection.

### Version 1 (Text)

```
PROXY TCP4 192.168.1.100 10.0.0.1 56324 443\r\n
```

Format: `PROXY <protocol> <src_ip> <dst_ip> <src_port> <dst_port>\r\n`

Supported protocols: `TCP4`, `TCP6`, `UNKNOWN`.

### Version 2 (Binary)

A binary protocol with a 12-byte signature followed by version, command, address family, transport protocol, and address data.

Supported address families: IPv4 (`AF_INET`), IPv6 (`AF_INET6`), `AF_UNSPEC`.

Commands: `PROXY` (0x01, with addresses) and `LOCAL` (0x00, health check, no addresses).

### PROXY Protocol Configuration

```kdl
global {
    proxy-protocol true
}
```

When enabled, Gatel expects a PROXY protocol header on **every** incoming TCP connection (both HTTP and HTTPS listeners).

### How PROXY Protocol Works

1. When a new connection is accepted, Gatel reads the first bytes to detect the protocol version.
2. **v2 detection**: The first 12 bytes are compared against the v2 binary signature (`\r\n\r\n\0\r\nQUIT\n`).
3. **v1 detection**: If the data starts with `PROXY `, it is parsed as a v1 text header.
4. **Fallback**: If neither is detected, the buffered bytes are prepended to the stream and processing continues without PROXY protocol.
5. The real client address from the header replaces the TCP peer address for all downstream processing.
6. A `PrefixedStream` wrapper ensures that any leftover bytes after the PROXY header are seamlessly fed into the HTTP/TLS parsing layer.

For HTTPS connections, the PROXY protocol header is parsed **before** the TLS handshake.

---

## Request Matchers

Matchers allow routes to be selected based on conditions beyond just the path pattern. Multiple `match` directives in a route form a logical AND -- all must match for the route to be selected.

### Method Matcher

Match requests by HTTP method. Accepts a comma-separated list.

```kdl
route "/api/*" {
    match method="GET,POST,PUT"
    proxy "127.0.0.1:3000"
}
```

Method comparison is case-insensitive.

### Path Matcher

Match by an additional path pattern (beyond the route's path pattern). Supports glob patterns.

```kdl
route "/*" {
    match path="/api/v2/*"
    proxy "127.0.0.1:3000"
}
```

Path patterns support:
- `*` -- matches any characters except `/`
- `**` -- matches any characters including `/`
- `?` -- matches any single character
- Exact strings -- match literally

### Header Matcher

Match requests that have a specific header with a value matching a glob pattern.

```kdl
route "/api/*" {
    match header="X-Api-Version" pattern="v2*"
    proxy "127.0.0.1:3000"
}
```

| Property | Default | Description |
|---|---|---|
| `header` | -- | Header name to check |
| `pattern` | `"*"` | Glob pattern to match against the header value |

### Query Matcher

Match requests by query parameter.

```kdl
// Match if query parameter "debug" exists (any value)
route "/*" {
    match query="debug"
    proxy "127.0.0.1:3001"
}

// Match if query parameter "format" equals "json"
route "/*" {
    match query="format" value="json"
    proxy "127.0.0.1:3001"
}
```

| Property | Description |
|---|---|
| `query` | Query parameter name to check |
| `value` | (Optional) Expected value. If omitted, only checks that the parameter exists. |

### Remote IP Matcher

Match requests by client IP address using CIDR notation.

```kdl
route "/*" {
    match remote-ip="192.168.0.0/16,10.0.0.0/8"
    proxy "127.0.0.1:3000"
}
```

Accepts a comma-separated list of CIDR ranges or exact IP addresses. Supports both IPv4 and IPv6.

### Protocol Matcher

Match by the request protocol/scheme.

```kdl
route "/*" {
    match protocol="https"
    proxy "127.0.0.1:3000"
}
```

Comparison is case-insensitive. Common values: `http`, `https`.

### Expression Matcher

A mini-expression language for combining conditions in a single directive.

```kdl
route "/*" {
    match expression="{method} == GET && {path} ~ /api/*"
    proxy "127.0.0.1:3000"
}
```

**Variables:**

| Variable | Value |
|---|---|
| `{method}` | HTTP method |
| `{path}` | Request path |
| `{host}` | Host header value |
| `{remote_ip}` | Client IP address |
| `{scheme}` / `{protocol}` | Request scheme |
| `{query}` | Query string |

**Operators:**

| Operator | Meaning | Example |
|---|---|---|
| `==` | Exact equality | `{method} == GET` |
| `!=` | Inequality | `{method} != DELETE` |
| `~` | Glob match | `{path} ~ /api/*` |

**Combinators:**

| Combinator | Meaning |
|---|---|
| `&&` | Logical AND |
| `\|\|` | Logical OR |

`||` has the lowest precedence (evaluated first as splits), then `&&`.

**Examples:**

```kdl
// API requests from internal network
match expression="{path} ~ /api/* && {remote_ip} == 10.0.0.0/8"

// Any method except DELETE
match expression="{method} != DELETE"

// Multiple paths
match expression="{path} ~ /api/* || {path} ~ /webhook/*"
```

### Not Matcher

Negate an inner matcher.

```kdl
route "/*" {
    match not {
        match method="DELETE"
    }
    proxy "127.0.0.1:3000"
}
```

The route matches when the inner matcher does **not** match.

### Combining Matchers

Multiple `match` directives on the same route form a logical AND:

```kdl
route "/api/*" {
    match method="GET,POST"
    match header="Authorization" pattern="Bearer *"
    match remote-ip="10.0.0.0/8"
    proxy "127.0.0.1:3000"
}
```

This route matches only when:
- The method is GET or POST, **AND**
- The Authorization header starts with "Bearer ", **AND**
- The client IP is in the 10.0.0.0/8 range.

For OR logic between matchers, use the expression matcher:

```kdl
route "/*" {
    match expression="{path} ~ /api/* || {path} ~ /graphql"
    proxy "127.0.0.1:3000"
}
```

---

## Server-Side Templates

See [Middleware - Templates](middleware.md#templates) for the full reference.

Templates are a middleware that processes `text/html` responses and replaces `{{placeholder}}` tags with request-context values. They are useful for injecting dynamic data into static HTML files served by the file server.

```kdl
route "/*" {
    templates root="/var/www/templates"
    root "/var/www/html"
    file-server
}
```

---

## Response Caching

See [Middleware - Response Caching](middleware.md#response-caching) for the full reference.

The cache middleware stores responses in memory using an LRU eviction policy, respects `Cache-Control` headers, and supports conditional requests via ETag and Last-Modified.

```kdl
route "/api/*" {
    cache max-entries=5000 max-age="600s"
    proxy "127.0.0.1:3000"
}
```

---

## Graceful Shutdown

Gatel coordinates graceful shutdown across all listeners and active connections.

### Shutdown Signals

| Signal | Platform | Action |
|---|---|---|
| `SIGINT` / `Ctrl+C` | All | Graceful shutdown |
| `SIGTERM` | Unix | Graceful shutdown |
| `SIGHUP` | Unix | Hot reload (not shutdown) |

### Shutdown Sequence

1. **Signal received**: The shutdown flag is set via a `tokio::sync::watch` channel.
2. **Accept loops stop**: All listener loops check the shutdown flag and stop accepting new connections.
3. **Drain**: The server waits for active connections to complete. Active connections are tracked via atomic counters with RAII guards (`ConnectionGuard`).
4. **Grace period**: If connections do not drain within the configured `grace-period` (default 30 seconds), they are abandoned.
5. **TLS cleanup**: The certon maintenance loop is stopped.
6. **QUIC endpoint**: If HTTP/3 is enabled, the QUIC endpoint sends a GOAWAY frame and closes.
7. **Exit**: The process exits with code 0.

### Configuration

```kdl
global {
    grace-period "30s"
}
```

---

## Hot Reload Internals

When a reload is triggered (SIGHUP or Admin API):

1. **Read**: The config file is re-read from disk.
2. **Parse**: The KDL config is parsed and validated. If parsing fails, the reload is aborted.
3. **TLS reload**: If TLS is configured, manual certificates are re-read and ACME domains are updated.
4. **Router rebuild**: A new `Router` is compiled from the new config with fresh middleware chains.
5. **Atomic swap**: Both the config and router are stored via `ArcSwap::store()`.
6. **Immediate effect**: New requests use the new config immediately. In-flight requests continue with the old config snapshot.

The reload is safe because:
- `ArcSwap` provides wait-free reads (no locks, no blocking).
- The old config is kept alive by `Arc` reference counting until all in-flight requests using it complete.
- If any part of the reload fails (e.g., a PEM file is unreadable), the error is logged and the previous config remains active.
