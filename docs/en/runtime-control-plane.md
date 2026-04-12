# Runtime Control Plane

This document describes Gatel's runtime service model, the admin API used to mutate it, the routing and drain semantics applied at runtime, and the boundary between generic proxy primitives and external deployment controllers.

## Scope

Gatel's runtime layer is intentionally narrow:

- It owns generic proxy data-plane primitives such as services, routes, target groups, targets, TLS bindings, health-gated activation, draining, and observability.
- It does not own deploy workflows, release/version selection, rollback policy, container naming, role semantics, or controller UX.

An external controller such as `kamalx` should translate deployment intent into runtime API calls against these primitives.

## Runtime Service Model

Runtime state is stored separately from the static KDL config and merged into the live router atomically.

Hierarchy:

1. `service`
2. `listener`
3. `route`
4. `target_group`
5. `target`

Stable identifiers:

- `service.id`
- `listener.id`
- `route.id`
- `target_group.id`
- `target.id`

Service responsibilities:

- Declares runtime hosts and optional runtime TLS policy.
- Owns listeners and routes.
- Provides the revision boundary used by optimistic concurrency.

Route responsibilities:

- Defines host/path scope.
- Defines additional request matchers.
- Selects the load-balancing policy.
- Owns one or more weighted target groups.

Target group responsibilities:

- Provides a group-level weight for split traffic.

Target responsibilities:

- Carries an address, weight, activation state, and drain timeout.

Target states:

- `warming`: registered but excluded from load balancing until health checks succeed.
- `active`: eligible to receive traffic.
- `draining`: excluded from new traffic, but existing in-flight work is allowed to finish until the drain deadline.
- `failed`: quarantined because activation or health checks failed.

Persistence and recovery:

- Runtime state is written to `*.runtime.json` beside the main config file.
- Writes are crash-safe: a temporary file is written first, then renamed atomically.
- On restart, Gatel restores the runtime snapshot, validates it, and rebuilds the live router before serving traffic.
- Corrupted runtime state is reported through the admin API and metrics instead of being applied silently.

## Admin API

The admin API is a resource-oriented HTTP API.

Authentication:

- `admin_auth_token`: full read/write access.
- `admin_read_token`: read-only access.
- `admin_write_token`: mutating access and read access.

Concurrency control:

- Mutating requests can use `If-Match: "<revision>"`.
- A stale revision is rejected instead of partially applying a write.

Core read endpoints:

- `GET /health`
- `GET /config`
- `GET /runtime/state`
- `GET /services`
- `GET /services/{service}`
- `GET /services/{service}/routes`
- `GET /services/{service}/routes/{route}`
- `GET /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `GET /upstreams`
- `GET /metrics`

Core write endpoints:

- `PUT /services/{service}`
- `PATCH /services/{service}`
- `DELETE /services/{service}`
- `PUT /services/{service}/listeners/{listener}`
- `PUT /services/{service}/routes/{route}`
- `PATCH /services/{service}/routes/{route}`
- `DELETE /services/{service}/routes/{route}`
- `PUT /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `PATCH /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `DELETE /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`

Mutation guarantees:

- Validation runs before commit.
- The merged runtime+static config is rebuilt as one unit.
- TLS reload and router swap happen in the same apply step.
- If apply fails, the persisted runtime file is rolled back to the previous snapshot.

## Traffic Split Semantics

Runtime routes are ordered deterministically. More specific path scopes win before broader scopes.

Within the same host/path scope:

- protocol scoping is evaluated first (`http` vs `https`)
- header/cookie/query/method matchers refine the candidate set
- ambiguous sibling combinations are rejected at write time

Supported split primitives:

- weighted target groups
- header-based canarying
- cookie-based canarying
- path-scoped traffic policies
- host-scoped traffic policies
- sticky routing by cookie, header, or hash input

Load-balancing policies available at runtime:

- `round_robin`
- `random`
- `weighted_round_robin`
- `ip_hash`
- `least_conn`
- `uri_hash`
- `header_hash`
- `cookie_hash`
- `first`
- `two_random_choices`

Observability:

- `gatel_route_matches_total`
- `gatel_backend_selections_total`
- runtime target state metrics
- debug tracing that explains why a route matched and which upstream was selected

## Drain Behavior

When a target enters `draining`:

1. it is removed from the active load-balancing set immediately
2. new requests stop selecting it
3. existing response streams and WebSocket tunnels keep their runtime activity guard
4. the target is removed from runtime state when either:
   - active runtime activity reaches zero, or
   - `drain_timeout` expires

Operational model:

- The timeout is configured per runtime target.
- Short requests usually disappear immediately because their activity count reaches zero quickly.
- Long-lived HTTP response streaming and WebSocket tunnels are tracked independently from router membership.
- Once the timeout expires, the target is removed from routing state even if an already-established stream or tunnel is still winding down.

This gives controllers a predictable cutover point without breaking in-flight work prematurely.

## Runtime TLS And Host Policy

Runtime TLS state can update:

- manual certificate/key references
- HTTPS redirect policy
- canonical host redirect policy

Validation:

- runtime/runtime SNI conflicts are rejected
- static/runtime host conflicts are rejected
- canonical hosts must be explicitly declared by the runtime service

Reload model:

- runtime TLS changes are merged into the live config snapshot
- the `TlsManager` reloads the merged config without a full process restart
- new connections see the new certificates and redirect policy immediately

Operational limits:

- runtime listeners are logical routing listeners, not OS-level bind/unbind operations
- runtime TLS requires that the HTTPS listener and TLS manager were configured at process start
- existing TLS handshakes and existing proxied streams are not retroactively rewritten

## Generic Proxy Primitive Boundary

Belongs in Gatel:

- runtime routing graph
- target activation and draining
- health checks
- traffic splitting and affinity
- runtime TLS and host redirects
- persistence, recovery, metrics, and audit logs

Belongs in an external controller:

- deploy/rollback commands
- release naming and selection
- app/accessory/role mapping
- container orchestration
- rollout policy and user-facing workflow

The controller should treat Gatel as a reusable proxy control plane, not as a deployment product with opinionated release semantics.
