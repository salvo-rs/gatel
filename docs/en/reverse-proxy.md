# Reverse Proxy

Gatel's reverse proxy forwards HTTP requests to upstream backend servers with support for load balancing, health checking, automatic retries, header manipulation, DNS-based dynamic upstreams, and transparent WebSocket proxying.

## Table of Contents

- [Basic Proxy](#basic-proxy)
- [Multiple Upstreams](#multiple-upstreams)
- [Load Balancing](#load-balancing)
  - [round_robin](#round_robin)
  - [random](#random)
  - [weighted_round_robin](#weighted_round_robin)
  - [ip_hash](#ip_hash)
  - [least_conn](#least_conn)
  - [uri_hash](#uri_hash)
  - [header_hash](#header_hash)
  - [cookie_hash](#cookie_hash)
  - [first](#first)
- [Active Health Checks](#active-health-checks)
- [Passive Health Checks](#passive-health-checks)
- [Retries](#retries)
- [Header Manipulation](#header-manipulation)
  - [header-up (Request Headers)](#header-up-request-headers)
  - [header-down (Response Headers)](#header-down-response-headers)
  - [Placeholders](#placeholders)
- [DNS Upstreams](#dns-upstreams)
- [WebSocket Proxying](#websocket-proxying)
- [Connection Pooling](#connection-pooling)
- [How It Works](#how-it-works)

---

## Basic Proxy

The simplest form forwards all requests to a single upstream:

```kdl
site "app.example.com" {
    route "/*" {
        proxy "127.0.0.1:3000"
    }
}
```

This creates a single-backend proxy with default settings (round robin, no health checks, no retries).

---

## Multiple Upstreams

Use the block form to define multiple upstream backends:

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    upstream "10.0.1.3:8080"
}
```

Each `upstream` node takes a `host:port` address as its argument. By default, traffic is distributed using round-robin load balancing.

### Weighted Upstreams

Assign weights to control traffic distribution:

```kdl
proxy {
    upstream "10.0.1.1:8080" weight=5
    upstream "10.0.1.2:8080" weight=3
    upstream "10.0.1.3:8080" weight=1
}
```

The default weight is 1. Weights are used by `weighted_round_robin` and influence the proportion of requests each backend receives.

---

## Load Balancing

Gatel supports nine load-balancing strategies. Set the strategy with the `lb` directive:

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    lb "round_robin"
}
```

All strategies skip unhealthy backends automatically.

### round_robin

```kdl
lb "round_robin"
```

Cycles through healthy backends in order. This is the **default** strategy.

- Stateful: uses an atomic counter.
- Even distribution across backends.
- Simple and predictable.

### random

```kdl
lb "random"
```

Selects a random healthy backend for each request.

- Stateless per request.
- Good distribution with many backends.

### weighted_round_robin

```kdl
lb "weighted_round_robin"
```

Smooth weighted round-robin (nginx-style). Each backend has an `effective_weight` and `current_weight`. On each selection, all healthy backends' `current_weight` is incremented by their `effective_weight`; the backend with the highest `current_weight` is selected and has the total weight subtracted.

This produces a smooth distribution. For example, with weights 5:3:1 the sequence avoids long runs of the same backend.

Requires `weight` on upstream nodes.

### ip_hash

```kdl
lb "ip_hash"
```

Computes a hash of the client IP address and consistently maps it to the same backend. Useful for session affinity -- the same client always reaches the same backend (as long as it remains healthy).

### least_conn

```kdl
lb "least_conn"
```

Selects the healthy backend with the fewest active connections. Active connections are tracked per-backend using atomic counters with RAII guards that decrement on drop.

Best for backends with varying response times.

### uri_hash

```kdl
lb "uri_hash"
```

Hashes the request URI path to select a backend. Requests to the same URL consistently hit the same backend, which is useful for caching layers.

### header_hash

```kdl
lb "header_hash" header="X-User-ID"
```

Hashes the value of a specified request header. If the header is missing, defaults to an empty string.

| Property | Default | Description |
|---|---|---|
| `header` | `"X-Forwarded-For"` | Name of the header to hash |

### cookie_hash

```kdl
lb "cookie_hash" cookie="session_id"
```

Hashes the value of a specified cookie for session affinity.

| Property | Default | Description |
|---|---|---|
| `cookie` | `"session"` | Name of the cookie to hash |

### first

```kdl
lb "first"
```

Always selects the first healthy backend. This implements an active/standby failover pattern -- traffic only goes to backup backends when earlier ones are unhealthy.

---

## Active Health Checks

Active health checks periodically send HTTP GET requests to each backend and track consecutive successes and failures against configurable thresholds.

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    health-check uri="/health" interval="10s" timeout="5s" unhealthy-threshold=3 healthy-threshold=2
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `uri` | string | `"/health"` | Path to probe on each backend |
| `interval` | duration | `"10s"` | Time between probes |
| `timeout` | duration | `"5s"` | Maximum time to wait for a response |
| `unhealthy-threshold` | integer | `3` | Consecutive failures before marking a backend unhealthy |
| `healthy-threshold` | integer | `2` | Consecutive successes before marking a backend healthy again |

**How it works:**

- A background task runs on a Tokio green thread for each proxy handler.
- Every `interval`, each backend receives a GET request to the `uri`.
- A 2xx response counts as a success; any other status or a timeout counts as a failure.
- After `unhealthy-threshold` consecutive failures, the backend is marked unhealthy and excluded from load balancing.
- After `healthy-threshold` consecutive successes, the backend is marked healthy again.
- The health checker task is aborted when the proxy handler is dropped (e.g., on config reload).

---

## Passive Health Checks

Passive health checks monitor actual traffic and temporarily disable backends that return too many server errors.

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    passive-health max-fails=5 fail-window="30s" cooldown="60s"
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `max-fails` | integer | `5` | Maximum 5xx responses within the window before disabling |
| `fail-window` | duration | `"30s"` | Sliding time window for counting failures |
| `cooldown` | duration | `"60s"` | How long a disabled backend stays down before automatic recovery |

**How it works:**

- Each upstream response is inspected. 5xx status codes and connection errors are recorded as failures.
- Failures are tracked in a timestamped ring buffer per backend.
- When the failure count within the `fail-window` reaches `max-fails`, the backend is marked unhealthy.
- After the `cooldown` period elapses, the backend is automatically re-enabled and its failure history is cleared.
- Active and passive health checks work together -- either one can mark a backend unhealthy.

---

## Retries

Gatel can retry failed requests on a different backend:

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    retries 2
}
```

| Directive | Default | Description |
|---|---|---|
| `retries` | `0` | Number of retry attempts after the initial request fails |

**Retry behavior:**

- The request body is buffered in memory so it can be replayed on retries.
- A request is retried when: (a) the upstream connection fails, or (b) the upstream returns a 5xx status code.
- On retry, the load balancer is called again to select a (potentially different) backend. If there are multiple backends, Gatel tries to avoid the backend that just failed.
- Total attempts = 1 (initial) + retries.
- If all attempts fail, the error from the last attempt is returned to the client.

---

## Header Manipulation

### header-up (Request Headers)

Modify headers sent to the upstream.

**Set a header:**

```kdl
header-up "X-Real-IP" "{client_ip}"
```

**Remove a header** (prefix the name with `-`):

```kdl
header-up "-X-Internal-Secret"
```

### header-down (Response Headers)

Modify headers returned to the client.

**Set a header:**

```kdl
header-down "X-Served-By" "gatel"
```

**Remove a header:**

```kdl
header-down "-Server"
```

### Placeholders

The `header-up` value field supports `{client_ip}`, which is replaced with the client's IP address at request time.

**Complete example:**

```kdl
proxy {
    upstream "10.0.1.1:8080"
    upstream "10.0.1.2:8080"
    header-up "X-Real-IP" "{client_ip}"
    header-up "X-Forwarded-Proto" "https"
    header-up "-X-Debug"
    header-down "-Server"
    header-down "-X-Powered-By"
    header-down "X-Frame-Options" "DENY"
}
```

---

## DNS Upstreams

For dynamic environments (e.g., Kubernetes), Gatel can resolve upstream addresses from DNS and periodically refresh them.

```kdl
proxy {
    upstream "fallback:8080"
    dns-upstream name="app.svc.cluster.local" port=8080 refresh="30s"
}
```

| Property | Type | Default | Description |
|---|---|---|---|
| `name` | string | -- | DNS name to resolve (A/AAAA records) |
| `port` | integer | `80` | Port to pair with resolved IP addresses |
| `refresh` | duration | `"30s"` | How often to re-resolve the DNS name |

**How it works:**

- A background task resolves the DNS name immediately at startup, then re-resolves every `refresh` interval.
- Resolved addresses are atomically swapped in using `ArcSwap`.
- If a DNS resolution returns zero results, the previous list is kept.
- Static `upstream` entries can be combined with `dns-upstream` as fallbacks.
- Each resolved IP gets a default weight of 1.

---

## WebSocket Proxying

Gatel transparently detects and proxies WebSocket connections. No configuration is needed -- it happens automatically when a request contains `Connection: Upgrade` and `Upgrade: websocket` headers.

**How it works:**

1. The load balancer selects a backend for the WebSocket connection.
2. Gatel opens a raw TCP connection to the upstream and sends the HTTP upgrade request.
3. The upstream's 101 Switching Protocols response is forwarded to the client.
4. A bidirectional byte-copy tunnel is established using `tokio::io::copy_bidirectional`.
5. The tunnel runs until either side closes the connection.

WebSocket connections are counted as active connections for the `least_conn` load balancer and for graceful shutdown tracking.

---

## Connection Pooling

Gatel uses `hyper-util`'s connection-pooling HTTP client. The same TCP connection is reused for multiple requests to the same upstream address (HTTP/1.1 keep-alive and HTTP/2 multiplexing).

- One shared `Client` instance per `UpstreamPool` (per proxy handler).
- Connections are established lazily and kept alive between requests.
- No explicit configuration is needed -- pooling is on by default.

---

## How It Works

The complete request flow through the reverse proxy:

1. **Routing**: The router matches the request host and path to a site and route.
2. **Middleware**: The request passes through the middleware chain (logging, rate limiting, compression, etc.).
3. **WebSocket check**: If the request is a WebSocket upgrade, it takes the dedicated WebSocket path.
4. **LB context**: The load balancer receives the client address, URI, and request headers.
5. **Backend selection**: The configured LB strategy picks a healthy backend.
6. **Body buffering**: The request body is read into memory (needed for retries).
7. **Request construction**: The upstream URI is built, the Host header is rewritten, and `header-up` directives are applied.
8. **Connection tracking**: An atomic counter is incremented (and decremented on drop via `ConnGuard`).
9. **Forward**: The request is sent to the upstream via the pooled HTTP client.
10. **Response processing**: The response is inspected for passive health (5xx tracking), `header-down` directives are applied.
11. **Retry (if needed)**: On upstream error or 5xx, the process repeats from step 5 with a different backend.
12. **Return**: The final response is sent back through the middleware chain to the client.
