# Configuration Reference

Gatel uses the [KDL](https://kdl.dev) document language for configuration. KDL is a node-based format that is concise, human-readable, and well-suited for hierarchical configuration.

## Table of Contents

- [File Format](#file-format)
- [Top-Level Blocks](#top-level-blocks)
- [global Block](#global-block)
  - [admin](#admin)
  - [log](#log)
  - [grace-period](#grace-period)
  - [http](#http)
  - [https](#https)
  - [http3](#http3)
  - [proxy-protocol](#proxy-protocol)
- [tls Block](#tls-block)
  - [acme](#acme)
  - [client-auth](#client-auth)
  - [on-demand](#on-demand)
- [site Block](#site-block)
  - [Per-Site tls](#per-site-tls)
  - [route](#route)
- [Route Directives](#route-directives)
  - [Matchers](#matchers)
  - [Middleware](#middleware)
  - [Handlers](#handlers)
- [stream Block](#stream-block)
- [Duration Format](#duration-format)
- [Address Format](#address-format)
- [Full Example](#full-example)

---

## File Format

KDL nodes have a name, optional positional arguments, optional named properties, and optional children enclosed in `{ }`.

```kdl
// This is a comment
node-name "positional-arg" named-prop="value" {
    child-node "arg"
}
```

Gatel reads the config file as a KDL document. The top-level nodes must be one of: `global`, `tls`, `site`, or `stream`. Unknown top-level nodes produce a parse error.

---

## Top-Level Blocks

| Block | Required | Description |
|---|---|---|
| `global` | No | Server-wide settings (listen addresses, logging, admin, shutdown). |
| `tls` | No | Global TLS and ACME settings. |
| `site` | Yes (at least one) | Virtual host definition. Takes a hostname as argument. |
| `stream` | No | L4 TCP stream proxy listeners. |

---

## global Block

Controls server-wide behavior. All directives are optional; sensible defaults are used when omitted.

```kdl
global {
    admin ":2019"
    log level="info" format="pretty"
    grace-period "30s"
    http ":80"
    https ":443"
    http3 true
    proxy-protocol true
}
```

### admin

```kdl
admin ":2019"
```

Starts the Admin REST API on the given address. When omitted, the admin API is disabled.

- **Type**: address string
- **Default**: disabled

### log

```kdl
log level="info" format="pretty"
```

Configures structured logging.

| Property | Type | Default | Values |
|---|---|---|---|
| `level` | string | `"info"` | `error`, `warn`, `info`, `debug`, `trace` |
| `format` | string | `"pretty"` | `pretty`, `json` |

### grace-period

```kdl
grace-period "30s"
```

Maximum time to wait for in-flight connections to drain during graceful shutdown.

- **Type**: duration string
- **Default**: `"30s"`

### http

```kdl
http ":80"
```

Listen address for the HTTP (plaintext) listener.

- **Type**: address string
- **Default**: `0.0.0.0:80`

### https

```kdl
https ":443"
```

Listen address for the HTTPS (TLS) listener. Only active when `tls` is configured.

- **Type**: address string
- **Default**: `0.0.0.0:443`

### http3

```kdl
http3 true
```

Enable the HTTP/3 (QUIC) listener on the same address as HTTPS. Requires the `http3` compile-time feature flag and a configured `tls` block.

- **Type**: boolean
- **Default**: `false`

### proxy-protocol

```kdl
proxy-protocol true
```

When enabled, Gatel expects a PROXY protocol v1 (text) or v2 (binary) header on every incoming TCP connection. The real client address from the header is used instead of the TCP peer address.

- **Type**: boolean
- **Default**: `false`

---

## tls Block

Global TLS settings. When present, Gatel starts an HTTPS listener. Sites without explicit per-site TLS certificates are enrolled in ACME management.

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
    client-auth required=true {
        ca-cert "/etc/gatel/client-ca.pem"
    }
    on-demand ask="https://auth.example.com/check" rate-limit=10
}
```

### acme

Configures automatic certificate issuance via ACME (e.g., Let's Encrypt).

```kdl
acme {
    email "admin@example.com"
    ca "letsencrypt"
    challenge "http-01"
}
```

| Directive | Required | Default | Description |
|---|---|---|---|
| `email` | Yes | -- | Contact email for the ACME account |
| `ca` | No | `letsencrypt` | Certificate authority: `letsencrypt`, `letsencrypt-staging`, `le`, `le-staging`, `zerossl` |
| `challenge` | No | `http-01` | Challenge type: `http-01` or `tls-alpn-01` |

Supported CAs:

| Config Value | CA | Notes |
|---|---|---|
| `letsencrypt` or `le` | Let's Encrypt (production) | Rate limits apply |
| `letsencrypt-staging` or `le-staging` | Let's Encrypt (staging) | For testing; issues untrusted certs |
| `zerossl` | ZeroSSL (production) | Alternative CA |

### client-auth

Configures mutual TLS (mTLS) client certificate verification.

```kdl
client-auth required=true {
    ca-cert "/etc/gatel/client-ca.pem"
    ca-cert "/etc/gatel/another-ca.pem"
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `required` | boolean | `true` | If `true`, connections without a valid client cert are rejected. If `false`, client certs are requested but not required. |

Child nodes:

| Node | Description |
|---|---|
| `ca-cert` | Path to a PEM file containing one or more CA certificates used to verify client certs. Multiple `ca-cert` nodes are allowed. |

### on-demand

Enables on-demand TLS: certificates are obtained automatically at TLS handshake time for previously unknown domains.

```kdl
on-demand ask="https://auth.example.com/check" rate-limit=10
```

| Property | Type | Default | Description |
|---|---|---|---|
| `ask` | string | -- | URL to query to decide if a domain is allowed. A GET request is made to `{ask}?domain={sni}`. A 200 response means the domain is allowed. |
| `rate-limit` | integer | -- | Maximum number of certificates to issue per minute. |

---

## site Block

Defines a virtual host. The positional argument is the hostname used for routing (matched against the `Host` header).

```kdl
site "app.example.com" {
    tls {
        cert "/etc/gatel/cert.pem"
        key "/etc/gatel/key.pem"
    }
    route "/api/*" {
        // middleware and handler directives
    }
    route "/*" {
        respond "Hello!" status=200
    }
}
```

Use `"*"` as the hostname for a catch-all site.

### Per-Site tls

Override the global TLS settings with manual PEM certificates for this site.

```kdl
tls {
    cert "/path/to/fullchain.pem"
    key "/path/to/privkey.pem"
}
```

| Directive | Required | Description |
|---|---|---|
| `cert` | Yes | Path to the certificate chain PEM file |
| `key` | Yes | Path to the private key PEM file |

Sites with manual certs take priority over ACME-managed certificates.

### route

Defines a route within a site. The positional argument is the path pattern.

```kdl
route "/api/*" {
    // middleware directives (processed in order)
    rate-limit window="1m" max=100
    encode "gzip" "zstd"

    // matcher directives (all must match)
    match method="GET,POST"

    // terminal handler (exactly one required)
    proxy "127.0.0.1:3000"
}
```

**Path patterns:**

| Pattern | Matches |
|---|---|
| `"/*"` or `"*"` | Everything (catch-all) |
| `"/api/*"` | `/api`, `/api/`, `/api/foo`, `/api/foo/bar` |
| `"/exact"` | Only `/exact` (exact match) |
| `"*.php"` | Any path ending with `.php` |

Routes are evaluated in specificity order (most specific first). Exact matches are most specific, followed by longer prefixes, then wildcards.

---

## Route Directives

Inside a `route` block, directives are categorized as **matchers**, **middleware**, or **handlers**.

### Matchers

Matchers add conditions that must pass (in addition to path matching) before a route is selected. Multiple `match` directives form a logical AND -- all must match.

```kdl
match method="GET,POST"
match header="X-Custom" pattern="foo*"
match query="key" value="val"
match remote-ip="192.168.0.0/16"
match protocol="https"
match expression="{method} == GET && {path} ~ /api/*"
match not {
    match method="DELETE"
}
```

| Matcher | Properties | Description |
|---|---|---|
| `method` | `method` (comma-separated) | Match HTTP methods |
| `path` | `path` (glob pattern) | Match the request path |
| `header` | `header`, `pattern` | Match a header value with glob pattern |
| `query` | `query`, `value` (optional) | Match a query parameter (presence or exact value) |
| `remote-ip` | `remote-ip` (comma-separated CIDRs) | Match client IP against CIDR ranges |
| `protocol` | `protocol` | Match the request scheme (`http` or `https`) |
| `expression` | `expression` | Evaluate a simple expression |
| `not` | (children) | Negate an inner matcher |

**Expression syntax:**

Variables: `{method}`, `{path}`, `{host}`, `{remote_ip}`, `{scheme}`, `{query}`

Operators: `==` (equals), `!=` (not equals), `~` (glob match)

Combinators: `&&` (AND), `||` (OR)

```kdl
match expression="{method} == GET && {path} ~ /api/*"
match expression="{remote_ip} == 127.0.0.1 || {remote_ip} == ::1"
```

### Middleware

Middleware directives are processed in the order they appear. They wrap the handler and can inspect/modify the request, modify the response, or short-circuit (e.g., return 429 for rate limiting).

| Directive | Description | Reference |
|---|---|---|
| `rate-limit` | Per-IP token bucket rate limiter | [Middleware](middleware.md#rate-limiting) |
| `encode` | Response compression (gzip, zstd, brotli) | [Middleware](middleware.md#compression) |
| `basic-auth` | HTTP Basic authentication | [Middleware](middleware.md#basic-authentication) |
| `cache` | LRU response cache | [Middleware](middleware.md#response-caching) |
| `templates` | Server-side HTML template processing | [Middleware](middleware.md#templates) |

Logging is always the outermost middleware (added automatically).

### Handlers

Every route must have exactly one terminal handler. The handler produces the final response.

#### proxy

Forward the request to upstream backend(s).

**Simple form** (single upstream):

```kdl
proxy "127.0.0.1:3000"
```

**Full form** (multiple upstreams with load balancing):

```kdl
proxy {
    upstream "127.0.0.1:3001" weight=3
    upstream "127.0.0.1:3002" weight=1
    lb "weighted_round_robin"
    health-check uri="/health" interval="10s" timeout="5s"
    passive-health max-fails=5 fail-window="30s" cooldown="60s"
    retries 2
    header-up "X-Real-IP" "{client_ip}"
    header-down "-Server"
    dns-upstream name="app.svc.cluster.local" port=8080 refresh="30s"
}
```

See [Reverse Proxy](reverse-proxy.md) for full details.

#### fastcgi

Forward requests via the FastCGI protocol (e.g., to PHP-FPM).

```kdl
fastcgi "127.0.0.1:9000" {
    root "/var/www/html"
    split ".php"
    index "index.php"
    env "APP_ENV" "production"
}
```

| Directive | Default | Description |
|---|---|---|
| `root` / `script-root` | `""` | Document root on the FastCGI server |
| `split` | -- | Path-info split marker (e.g., `".php"`) |
| `index` | `"index.php"` | Index filenames for directory requests. Multiple allowed. |
| `env` | -- | Extra environment variables (two positional args: key and value) |

#### file-server

Serve static files from the filesystem.

```kdl
root "/var/www/html"
file-server browse=true
```

The `root` directive sets the base directory. `file-server` activates the handler.

| Property | Type | Default | Description |
|---|---|---|---|
| `browse` | boolean | `false` | Enable directory listing when no index.html exists |

Features: MIME detection, ETag, Last-Modified, conditional requests (304), byte-range requests, directory index (index.html).

#### redirect

Redirect the client to another URL.

```kdl
redirect "https://example.com{path}" permanent=true
```

| Property | Type | Default | Description |
|---|---|---|---|
| `permanent` | boolean | `false` | If `true`, uses 301 (Moved Permanently). If `false`, uses 307 (Temporary Redirect). |

The target URL supports `{path}` and `{query}` placeholders.

#### respond

Return a fixed response.

```kdl
respond "Hello, World!" status=200
```

| Property | Type | Default | Description |
|---|---|---|---|
| `status` | integer | `200` | HTTP status code |

---

## stream Block

Defines L4 TCP stream proxy listeners for non-HTTP protocols.

```kdl
stream {
    listen ":3306" {
        proxy "mysql-server:3306"
    }
    listen ":6379" {
        proxy "redis-server:6379"
    }
}
```

Each `listen` node takes an address and contains a `proxy` directive with the upstream target address. Gatel performs bidirectional byte copying between the client and upstream.

---

## Duration Format

Durations are specified as strings with a unit suffix:

| Format | Example | Meaning |
|---|---|---|
| `Ns` | `"30s"` | N seconds |
| `Nm` | `"5m"` | N minutes |
| `Nh` | `"2h"` | N hours |
| `N` | `"30"` | N seconds (bare number) |

---

## Address Format

Listen addresses use the format `"host:port"` or `":port"` (binds to `0.0.0.0`):

| Format | Resolves to |
|---|---|
| `":8080"` | `0.0.0.0:8080` |
| `"127.0.0.1:3000"` | `127.0.0.1:3000` |
| `"0.0.0.0:443"` | `0.0.0.0:443` |

---

## Full Example

```kdl
global {
    admin ":2019"
    log level="info" format="json"
    grace-period "30s"
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
    route "/api/*" {
        match method="GET,POST,PUT,DELETE"
        rate-limit window="1m" max=100
        encode "gzip" "zstd" "brotli"
        proxy {
            upstream "10.0.1.1:8080" weight=3
            upstream "10.0.1.2:8080" weight=1
            lb "weighted_round_robin"
            health-check uri="/health" interval="10s" timeout="5s" unhealthy-threshold=3 healthy-threshold=2
            passive-health max-fails=5 fail-window="30s" cooldown="60s"
            retries 2
            header-up "X-Real-IP" "{client_ip}"
            header-down "-Server"
        }
    }
    route "/static/*" {
        encode "gzip" "zstd"
        cache max-entries=5000 max-age="1h"
        root "/var/www/static"
        file-server
    }
    route "/*" {
        redirect "https://app.example.com/api/" permanent=false
    }
}

site "api.internal.com" {
    tls {
        cert "/etc/gatel/internal-cert.pem"
        key "/etc/gatel/internal-key.pem"
    }
    route "/*" {
        basic-auth {
            user "admin" hash="$2b$12$LJ3m4ys3Rl4Kv1Q8xW5Yz.abc123..."
        }
        proxy "localhost:9090"
    }
}

site "php.example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/php"
            split ".php"
            index "index.php"
            env "APP_ENV" "production"
        }
    }
    route "/*" {
        root "/var/www/php"
        file-server
    }
}

stream {
    listen ":3306" {
        proxy "mysql-primary:3306"
    }
}
```
