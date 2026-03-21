# Middleware Reference

Middleware in Gatel intercepts requests before they reach the terminal handler and can modify requests, modify responses, or short-circuit the chain entirely. Middleware is configured per-route and executes in the order it appears in the configuration.

## Table of Contents

- [Middleware Chain](#middleware-chain)
- [Logging](#logging)
- [Rate Limiting](#rate-limiting)
- [Compression](#compression)
- [Basic Authentication](#basic-authentication)
- [IP Filtering](#ip-filtering)
- [Header Manipulation](#header-manipulation)
- [URI Rewriting](#uri-rewriting)
- [Response Caching](#response-caching)
- [Templates](#templates)
- [File Server](#file-server)
- [Metrics](#metrics)

---

## Middleware Chain

Every route compiles into a middleware chain: an ordered list of middleware followed by a terminal handler.

```
Request -> [Logging] -> [IP Filter] -> [Rate Limit] -> [Auth] -> [Rewrite]
        -> [Headers] -> [Compress] -> [Cache] -> [Templates] -> Handler -> Response
```

Logging middleware is always added as the outermost layer, regardless of the config. The remaining middleware runs in the order declared in the configuration file.

Each middleware receives the request context and a `Next` object representing the rest of the chain. It can:

1. Pass the request to `next.run(ctx)` unmodified.
2. Modify the request before calling `next.run(ctx)`.
3. Modify the response returned by `next.run(ctx)`.
4. Short-circuit by returning a response without calling `next` (e.g., 429, 401, 403).

---

## Logging

Logging middleware is **always active** and automatically wraps every route. It logs the method, path, status code, client address, and latency for every request.

There is no configuration directive for logging middleware -- it is unconditional.

**Log output (pretty format):**

```
INFO request handled client=127.0.0.1:52340 method=GET path=/api/users status=200 latency_ms=12
```

**Log output (JSON format):**

```json
{"level":"INFO","fields":{"client":"127.0.0.1:52340","method":"GET","path":"/api/users","status":200,"latency_ms":12},"message":"request handled"}
```

On error, the middleware logs the error message instead of the status code.

---

## Rate Limiting

Per-IP rate limiting using a token bucket algorithm.

```kdl
route "/api/*" {
    rate-limit window="1m" max=100
    proxy "127.0.0.1:3000"
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `window` | duration | `"60s"` | Time window for the token bucket |
| `max` | integer | `100` | Maximum requests allowed within the window |

### How It Works

Each unique client IP gets a token bucket with `max` tokens. Tokens are replenished at a rate of `max / window` per second. When a request arrives:

1. The bucket is refilled based on elapsed time since last access.
2. If at least one token is available, it is consumed and the request proceeds.
3. If no tokens are available, a `429 Too Many Requests` response is returned with a `Retry-After` header.

**Implementation details:**

- Buckets are stored in a `DashMap<IpAddr, TokenBucket>` for lock-free concurrent access.
- A background cleanup task runs periodically (at least every 60 seconds) to evict idle entries older than 2x the window.
- The token bucket is continuous (fractional tokens), not discrete, for smooth rate enforcement.

### Example: Strict API Rate Limit

```kdl
route "/api/*" {
    rate-limit window="10s" max=10
    proxy "127.0.0.1:3000"
}
```

This allows 10 requests per 10-second window per client IP (1 request/second sustained, with bursting).

---

## Compression

Response compression middleware. Inspects the `Accept-Encoding` request header and compresses the response body using the best mutually supported algorithm.

```kdl
route "/*" {
    encode "gzip" "zstd" "brotli"
    proxy "127.0.0.1:3000"
}
```

The positional arguments specify the enabled algorithms in preference order. If no arguments are provided, all three algorithms are enabled with the default preference order: zstd, brotli, gzip.

### Supported Algorithms

| Algorithm | Encoding Name | Typical Ratio | Speed |
|---|---|---|---|
| Zstandard | `zstd` | Best | Fastest |
| Brotli | `brotli` (or `br`) | Best | Slowest |
| Gzip | `gzip` | Good | Fast |

### Behavior

- **Minimum size**: Responses smaller than 256 bytes are not compressed.
- **Content-Type filter**: Only compressible MIME types are compressed. This includes:
  - All `text/*` types
  - `application/json`, `application/javascript`, `application/xml`
  - `application/wasm`, `image/svg+xml`
  - And many more application-specific types
- **Already compressed**: Responses with an existing `Content-Encoding` header are skipped.
- **Header updates**: The middleware sets `Content-Encoding`, removes the old `Content-Length`, and sets a new `Content-Length` matching the compressed size.

### Example: Gzip-Only Compression

```kdl
route "/api/*" {
    encode "gzip"
    proxy "127.0.0.1:3000"
}
```

---

## Basic Authentication

HTTP Basic authentication middleware. Validates the `Authorization: Basic <base64>` header against configured users.

```kdl
route "/admin/*" {
    basic-auth {
        user "admin" hash="$2b$12$LJ3m4ys3Rl4Kv1Q8xW5Yz.abcdef..."
        user "editor" hash="$2b$12$..."
    }
    proxy "127.0.0.1:3000"
}
```

### User Configuration

Each `user` node inside `basic-auth` takes:

- A positional argument: the username.
- A `hash` property: the password hash (or plaintext password).

### Password Storage

| Format | Prefix | Requirement |
|---|---|---|
| Bcrypt hash | `$2b$`, `$2a$`, `$2y$` | Requires the `bcrypt` feature flag |
| Plaintext | Anything else | Always available (not recommended for production) |

**Generating a bcrypt hash:**

```bash
# Using htpasswd
htpasswd -nbBC 12 "" your-password | cut -d: -f2

# Using Python
python3 -c "import bcrypt; print(bcrypt.hashpw(b'your-password', bcrypt.gensalt(12)).decode())"
```

### Behavior

- On missing or invalid `Authorization` header: returns `401 Unauthorized` with a `WWW-Authenticate: Basic realm="gatel"` header.
- On authentication failure: returns `401 Unauthorized`.
- On success: the request continues through the middleware chain.
- Plaintext password comparison uses constant-time byte comparison to prevent timing attacks.
- The `bcrypt` feature flag must be enabled at compile time to use bcrypt hashes. Without it, bcrypt hashes are always rejected.

---

## IP Filtering

CIDR-based IP allow/deny filtering.

IP filtering is configured as a middleware type in the codebase (`HoopConfig::IpFilter`) with `allow` and `deny` lists of CIDR ranges. Both IPv4 and IPv6 CIDR ranges are supported, as well as bare IP addresses.

### Behavior

- The **deny** list is checked first. If the client IP matches any deny entry, the request is blocked with `403 Forbidden`.
- If an **allow** list is configured, the client IP must match at least one allow entry (deny-by-default).
- If only a **deny** list is configured, all IPs are allowed except those explicitly denied.
- IPv4-mapped IPv6 addresses are handled transparently.

### CIDR Format

| Format | Meaning |
|---|---|
| `192.168.1.0/24` | All IPs in 192.168.1.0 - 192.168.1.255 |
| `10.0.0.0/8` | All IPs in 10.0.0.0 - 10.255.255.255 |
| `192.168.1.100` | Exact IP (treated as /32) |
| `::1` | IPv6 localhost (treated as /128) |
| `fd00::/8` | IPv6 private range |

---

## Header Manipulation

Request and response header manipulation middleware. Supports setting, adding, and removing headers with placeholder expansion.

The `HeadersMiddleware` operates on three sets:

1. **Request headers** (`request_set`): Headers set on the request before forwarding.
2. **Response headers** (`response_set`): Headers set on the response before returning to the client.
3. **Response removals** (`response_remove`): Headers removed from the response.

### Placeholders

Header values can contain placeholders that are expanded at request time:

| Placeholder | Value |
|---|---|
| `{client_ip}` | Client socket IP address |
| `{host}` | Request `Host` header value |
| `{method}` | HTTP method (GET, POST, etc.) |
| `{path}` | Request URI path |
| `{scheme}` | Request scheme (http/https) |

> **Note**: The proxy-specific `header-up` and `header-down` directives in the `proxy` block also support header manipulation. See [Reverse Proxy - Header Manipulation](reverse-proxy.md#header-manipulation).

---

## URI Rewriting

Modifies the request URI before it reaches the handler.

The `RewriteMiddleware` supports two modes:

### Strip Prefix

Removes a path prefix from the request URI:

```
strip_prefix = "/api"
/api/users?q=1 -> /users?q=1
/api           -> /
```

The prefix is only stripped on segment boundaries -- `/api` does not strip `/apifoo`.

### URI Template

Rewrites the entire URI using a template with placeholders:

| Placeholder | Value |
|---|---|
| `{path}` | Current request path (after any strip_prefix) |
| `{query}` | Current query string |

---

## Response Caching

In-memory LRU response cache that respects `Cache-Control` semantics.

```kdl
route "/api/*" {
    cache max-entries=1000 max-age="300s" max-entry-size=10485760
    proxy "127.0.0.1:3000"
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `max-entries` | integer | `1000` | Maximum number of cached responses |
| `max-entry-size` | integer | `10485760` (10 MB) | Maximum size of a single cached response body |
| `max-age` | duration | `"300s"` (5 min) | Default TTL when the response has no `Cache-Control` header |

### Cacheable Requests

Only `GET` and `HEAD` requests are cached. Requests with `Cache-Control: no-store` bypass the cache entirely.

### Cacheable Responses

Responses are cached when:

- Status code is 200 (OK), 301 (Moved Permanently), 302 (Found), or 304 (Not Modified).
- Response does not contain a `Set-Cookie` header.
- Response does not contain `Cache-Control: no-store`.
- Response body size does not exceed `max-entry-size`.

### TTL Determination

The cache TTL is determined in this order:

1. `s-maxage` from the response `Cache-Control` header.
2. `max-age` from the response `Cache-Control` header.
3. The configured `max-age` default.

If the resulting TTL is zero, the response is not cached.

### Conditional Requests

The cache supports conditional requests:

- **ETag / If-None-Match**: If the client sends an `If-None-Match` header matching the cached ETag, a `304 Not Modified` response is returned without the body.
- **Last-Modified / If-Modified-Since**: If the client sends an `If-Modified-Since` header matching the cached `Last-Modified`, a `304 Not Modified` is returned.

### Vary Header

The cache includes `Vary` header values in the cache key. Responses with different `Vary`-referenced header values are stored separately.

### Cache Eviction

When the cache reaches `max-entries`, the least recently used (LRU) entry is evicted. Expired entries are removed lazily on access.

### Response Headers

Cached responses include an `Age` header indicating how many seconds the response has been in the cache.

---

## Templates

Server-side HTML template processing middleware. Intercepts `text/html` responses and replaces `{{placeholder}}` tags with values from the request context.

```kdl
route "/*" {
    templates root="/templates"
    file-server
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `root` | string | CWD | Root directory for `{{include}}` file paths |

### Supported Placeholders

| Tag | Value |
|---|---|
| `{{host}}` | Request Host header |
| `{{path}}` | Request URI path |
| `{{method}}` | HTTP method |
| `{{scheme}}` | `https` or `http` |
| `{{client_ip}}` | Client IP address |
| `{{query}}` | Query string (without leading `?`) |
| `{{uri}}` | Full request URI |
| `{{remote_addr}}` | Full client socket address (IP:port) |
| `{{server_name}}` | Server hostname (Host header, without port) |
| `{{.Env.VARNAME}}` | Environment variable lookup |
| `{{include "path"}}` | Include another file's contents |

### Behavior

- Only processes responses with `Content-Type: text/html`.
- Maximum response size for template processing: 1 MB. Larger responses pass through unmodified.
- Non-UTF-8 response bodies pass through unmodified.
- `{{include}}` paths are resolved relative to the configured `root` directory.
- Path traversal (`..`) in include paths is blocked for security.
- Unknown tags are preserved as-is (no data loss).
- The `Content-Length` header is updated after processing.

### Example: Dynamic HTML Page

```html
<!DOCTYPE html>
<html>
<head><title>{{server_name}}</title></head>
<body>
  <p>Welcome, your IP is {{client_ip}}</p>
  <p>Environment: {{.Env.APP_ENV}}</p>
  {{include "partials/footer.html"}}
</body>
</html>
```

---

## File Server

Static file server handler. Serves files from a configured root directory.

```kdl
route "/static/*" {
    root "/var/www/static"
    file-server browse=true
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `browse` | boolean | `false` | Enable HTML directory listing when no index file exists |

### Features

- **MIME detection**: Determines Content-Type from file extension. Supports 40+ common types including HTML, CSS, JavaScript, JSON, images, fonts, audio, video, archives, and WebAssembly.
- **ETag**: Generated from the file's modification time and size. Format: `"<mtime_hex>-<size_hex>"`.
- **Last-Modified**: Set from the file's modification timestamp, formatted as an HTTP date.
- **Conditional requests**:
  - `If-None-Match`: Returns `304 Not Modified` if the ETag matches.
  - `If-Modified-Since`: Returns `304 Not Modified` if the file has not been modified since the given date.
- **Range requests**: Supports `Range: bytes=start-end` for partial content delivery (206 Partial Content). Handles explicit ranges, open-ended ranges (`500-`), and suffix ranges (`-500`).
- **Directory index**: Automatically serves `index.html` when a directory is requested.
- **Directory browsing**: When `browse=true` and no index file exists, generates a styled HTML directory listing with file names, sizes, and modification dates.
- **Path traversal prevention**: `..` components in request paths are rejected with `403 Forbidden`. Paths are percent-decoded before validation.

### Directory Listing

When browsing is enabled, directory listings include:
- Directories first, then files, sorted alphabetically.
- File sizes in human-readable format (KB, MB, GB).
- Modification dates in HTTP date format.
- A parent directory link (`..`) unless at the root.
- Clean, responsive CSS styling.

---

## Metrics

Prometheus-compatible metrics collection. Tracks request counts, latency, and active connections using lock-free atomics.

Metrics are exposed via the Admin API at `GET /metrics` (see [Getting Started - Admin API](getting-started.md#admin-api)).

### Exported Metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `gatel_requests_total` | counter | `host`, `method`, `status` | Total HTTP requests |
| `gatel_request_duration_seconds_sum` | histogram (sum) | `host`, `method` | Cumulative request processing time |
| `gatel_request_duration_seconds_count` | histogram (count) | `host`, `method` | Number of requests (for computing averages) |
| `gatel_active_connections` | gauge | -- | Current number of active connections |

### Example Prometheus Scrape Config

```yaml
scrape_configs:
  - job_name: 'gatel'
    static_configs:
      - targets: ['localhost:2019']
    metrics_path: '/metrics'
```
