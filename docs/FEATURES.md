# Features

What Gatel 2.0 must do, what it does, and what proves it.

A row is **done** only when something automated fails if the behaviour
regresses. "Implemented" without that is listed as **partial** and says what is
missing.

Run `gatel capabilities` against a build to see what that binary actually
carries; it is the same list, generated rather than written down.

## Configuration

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| KDL frontend, one file or many via `import` | yes | done | `gatel-kdl` load tests: glob, `optional`, cycles, `..` refused |
| Every unknown node, property and reference is an error with a position | yes | done | `gatel-plan/tests/normalize.rs` |
| Suggestions for near-misses | yes | done | `gatel-model` `did_you_mean` |
| `Snapshot → Spec → Plan → Generation → State` as the only path | yes | done | enforced by `cargo run -p xtask`; no second entry point exists |
| `spec_hash` ignores formatting; `plan_hash` includes the build's capabilities | yes | done | `gatel-plan` hash tests, `gatel-http/tests/reload.rs` no-op detection |
| Secrets by reference only (`env:`, `file:`), redacted everywhere | yes | done | `gatel-model/src/secret.rs`; `basic-auth` refuses an inline hash |
| `validate`, `explain`, `diff`, `capabilities`, `trust` | yes | done | exit codes covered by `crates/gatel/src/main.rs`; every example compiles in `gatel-http/tests/examples.rs` |
| Admin API as a second source under `runtime/` | no (2.1) | not started | — |
| Kubernetes source | no | not started | — |

## Reload

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Transactional commit: rejected, prepare-failed and stale all leave the running configuration untouched | yes | done | `gatel-plan/src/generation.rs`, `gatel-http/tests/reload.rs` |
| Last known good: a bad edit keeps serving the previous generation | yes | done | `gatel-http/tests/reload.rs` |
| No-op detection: reformatting a file consumes no generation | yes | done | `gatel-http/tests/reload.rs` |
| Sockets outlive generations; adding addresses is all-or-nothing | yes | done | `gatel-http/src/bind.rs`, `tests/reload.rs` |
| A connection keeps the generation it was accepted under | yes | done | `gatel-http/src/serve.rs`, `tests/reload.rs` |
| State survives a compatible reload (endpoints, pools, selectors, rate limits, certificates, metrics) | yes | done | `gatel-state/src/registry.rs`, `tests/observe.rs`, `tests/directives.rs` |
| File watching with a polling fallback, and signals | yes | done | `crates/gatel/src/watch.rs` |
| Reload stability matrix across every state kind | yes | done | `gatel-http/tests/stability.rs`, one row per cell of the table in `ARCHITECTURE.md` |

## Serving

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| HTTP/1.1 and HTTP/2, keep-alive, graceful drain | yes | done | `gatel-http/tests/serving.rs` |
| WebSocket and other upgrades, with idle and lifetime bounds | yes | done | `gatel-http/tests/upgrade.rs` |
| Host, path, method, header, query, cookie and client-IP matching | yes | done | `gatel-http/src/dispatch.rs`, `gatel-plan/src/matcher.rs` |
| Routes ordered by specificity; an ambiguous pair is a compile error | yes | done | `gatel-plan/tests/normalize.rs` |
| A site's own directives cover requests that matched no route | yes | done | `gatel-http/tests/observe.rs` |
| Request head, body, handshake, idle and connection limits, all applied | yes | done | `gatel-http/tests/limits.rs` |
| HTTP/3 | no | not started | — |
| Layer-4 TCP proxying | no | not started | — |

## Reverse proxy

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Ten balancing strategies | yes | done | `gatel-state/src/selector.rs` |
| Bounded connection pool with reuse, and pre-send checkout failures | yes | done | `gatel-state/src/pool.rs` |
| Per-stage timeouts | yes | done | `gatel-state/src/pool.rs`, `gatel-http/src/proxy.rs` |
| Active health checks, TCP and HTTP | yes | done | `gatel-state/src/supervisor.rs`, `probe.rs` |
| Passive health with ejection | yes | done | `gatel-state/src/endpoint.rs`, `gatel-http/tests/proxy.rs` |
| Safe retry: only a request with no body, only before anything was sent | yes | done | `gatel-http/src/proxy.rs` |
| Hop-by-hop headers stripped by rebuilding the request | yes | done | `gatel-http/src/proxy.rs` |
| `X-Forwarded-For` from a canonical client address, decided once | yes | done | `gatel-http/tests/proxy.rs` |
| Retry with a replayed body | no | not started | only `safe` is implemented, deliberately |
| Circuit breaker as a directive | no (2.1) | not started | passive ejection covers the common case |
| Forward proxying | no | not started | — |

## TLS

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Manual PEM certificates with SNI | yes | done | `gatel-http/tests/tls.rs` |
| Mutual TLS | yes | done | `gatel-tls/src/server.rs` |
| Certificates outlive generations; a renewal produces none | yes | done | `gatel-tls/src/store.rs` |
| A development authority, and `gatel trust` as its own command | yes | done | `gatel-tls/src/internal.rs`, `gatel-http/tests/tls.rs` |
| A local name served over TLS with nothing said about certificates | yes | done | `gatel-plan/tests/normalize.rs`, `gatel-http/tests/tls.rs` |
| A public name in the same position refused at compile time | yes | done | `gatel-http/tests/tls.rs` |
| No `tls-skip-verify`, anywhere | yes | done | the option does not exist |
| ACME with HTTP-01 | yes | done | `gatel-http/tests/acme.rs` drives a whole order — the real client, a test authority that actually fetches the challenge from the gateway |
| ACME with TLS-ALPN-01 or DNS-01 | no (2.1) | not started | `certon` implements both; neither is wired to a listener yet |
| Never ordering during a reload | yes | done | `gatel-tls/src/server.rs`, `tests/acme.rs`: a listener whose site has no certificate still starts |
| A certificate kept across a restart | yes | done | `gatel-http/tests/acme.rs` |
| On-demand certificates | no | not started | — |

## Directives

| Directive | Required for 2.0 | Status | Proof |
|---|---|---|---|
| `respond`, `redirect` | yes | done | unit tests in each module |
| `file-server` — containment, symlinks, ETag, conditional requests, precompressed | yes | done | `gatel-bundle-std/src/file_server.rs`, `gatel-http/tests/directives.rs` |
| `headers` | yes | done | unit tests; framing headers are protected |
| `encode` — gzip, brotli, zstd, deflate, streaming, `Vary`, minimum size | yes | done | `gatel-bundle-std/src/encode.rs`, `tests/directives.rs` |
| `rewrite` | yes | done | unit tests; runs after the guards |
| `access-log` — text, JSON, CLF, stderr or a rolled file | yes | done | `gatel-observe/src/access.rs`, `gatel-http/tests/observe.rs` |
| `request-id` | yes | done | unit tests |
| `basic-auth` — bcrypt, hashes by reference only | yes | done | `gatel-http/tests/auth.rs` |
| `ip-filter` | yes | done | unit tests, `tests/guards.rs` |
| `rate-limit` — typed state kept across a reload | yes | done | `tests/directives.rs` |
| `cors` | yes | done | `gatel-bundle-std/src/cors.rs`, `tests/guards.rs` |
| `timeout` | yes | done | `tests/guards.rs` |
| `force-https` | yes | done | `tests/guards.rs` |
| `trailing-slash` | yes | done | `tests/guards.rs` |
| `body-limit` | yes | done | `tests/guards.rs` |
| `error-pages` | yes | done | `tests/guards.rs` |
| `metrics`, `health` | yes | done | `gatel-http/tests/observe.rs` |
| `forward-auth` | no (2.1) | not started | needs controlled outbound requests, which belongs with the proxy |
| `cache` | no (2.1) | not started | — |
| Argon2, scrypt and PBKDF2 for `basic-auth` | no | not started | bcrypt covers what people have |
| The CGI family | no | not started | deliberately after 2.0 |

## Observability

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Prometheus metrics whose series come from the plan, never from a request | yes | done | `gatel-observe/src/series.rs`, `gatel-http/tests/observe.rs` |
| A request counted where its body ends, not where the handler returned | yes | done | `gatel-http/src/measure.rs` |
| Readiness that distinguishes local conditions from shared ones | yes | done | `gatel-observe/src/health.rs`, `tests/observe.rs` |
| Liveness that cannot fail on a downstream condition | yes | done | `gatel-observe/src/health.rs` |
| Metrics and health as ordinary routes, subject to their own guards | yes | done | `tests/observe.rs` |
| Structured events with stable names | yes | done | `docs/EVENTS.md`, checked against the code in both directions by `cargo run -p xtask` |
| OpenTelemetry traces | no | not started | — |

## Engineering

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Layering enforced rather than documented | yes | done | `cargo run -p xtask` |
| File length ceilings enforced | yes | done | `cargo run -p xtask` |
| Every example validated | yes | done | `gatel-http/tests/examples.rs` compiles every one, and refuses a secret written in place |
| Three-platform CI | yes | done | `.github/workflows/ci.yml`: Linux, macOS and Windows, plus an MSRV check |
| Fuzzing of the KDL frontend and the matchers | yes | **not started** | — |
| A soak test | yes | **not started** | — |
| Interoperability suite against curl, browsers and other proxies | yes | partial | tests speak raw HTTP on the wire; there is no external-client suite |
| Every event name written down and checked | yes | done | `docs/EVENTS.md`, `cargo run -p xtask` |
| Release matrix and signed artefacts | yes | **not started** | — |
| `gatel upgrade` | no | not started | — |

## What must land before 2.0 can be called done

In order of what a deployment would miss first:

1. **ACME against a real certificate authority.** The protocol is `certon`'s
   and the integration is covered end to end, but nothing here has met Let's
   Encrypt's staging environment. That is the next thing to do, and it needs a
   real domain rather than a test.
2. **Release engineering** — the matrix, signed artefacts, a soak test, fuzzing.
3. **Interoperability against real clients** — the tests speak raw HTTP, which
   is not the same as having been through a browser and a CDN.

Everything marked 2.1 is deliberate: `cache`, `forward-auth`, `circuit-breaker`,
the other ACME challenge types and the Admin API are each large enough to want
their own release, and none of them is needed to put this in front of a service.

## Where the ACME protocol comes from

`certon`, which is the same organisation, the same licence, and already what v1
uses — one implementation rather than two. Only its issuer is used, not its
certificate manager: Gatel already owns a certificate store, a resolver, a
background supervisor and a route table, and adopting a second set would split
"a renewal is not a configuration change" across two places that both believe
it.
