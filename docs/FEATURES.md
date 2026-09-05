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
| One cause reports as one message, not as the errors it caused | yes | done | `gatel-plan/tests/normalize.rs` |
| Suggestions for near-misses | yes | done | `gatel-model` `did_you_mean` |
| `Snapshot → Spec → Plan → Generation → State` as the only path | yes | done | enforced by `cargo run -p xtask`; no second entry point exists |
| `spec_hash` ignores formatting; `plan_hash` includes the build's capabilities | yes | done | `gatel-plan` hash tests, `gatel-http/tests/reload.rs` no-op detection |
| Secrets by reference only (`env:`, `file:`), redacted everywhere | yes | done | `gatel-model/src/secret.rs`; `basic-auth` refuses an inline hash |
| A plan carries the reference, never the material; an error never quotes it | yes | done | `gatel-http/tests/secrets.rs`, verified to fail when a leak is introduced |
| Provenance and clean-room rules written down | yes | done | `docs/design/provenance.md` |
| Threat model, with which defences are tested and which are not | yes | done | `docs/design/threat-model.md` |
| `validate`, `explain`, `diff`, `capabilities`, `trust` | yes | done | exit codes covered by `crates/gatel/src/main.rs`; every example compiles in `gatel-http/tests/examples.rs` |
| Admin API as a second source under `runtime/` | no (2.1) | not started | the `admin` block is refused at compile time rather than accepted and ignored: `gatel-plan/tests/normalize.rs` |
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
| PROXY protocol v1 and v2, so a layer-4 balancer can say who the client is | yes | done | `gatel-http/src/proxy_protocol.rs`, `tests/proxy_protocol.rs`; read only from a `trusted-proxy`, and refused at compile time without one |
| HTTP/3 | no | not started | — |
| Layer-4 TCP proxying | no | not started | — |

## Reverse proxy

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Ten balancing strategies | yes | done | `gatel-state/src/selector.rs` |
| Bounded connection pool with reuse, and pre-send checkout failures | yes | done | `gatel-state/src/pool.rs` |
| Per-stage timeouts: queue, connect, response head, body gap, overall | yes | done | every one of them applied, including `body-idle`, which bounds the gap between response chunks so a backend that answers and then stalls is cut off rather than held until `overall`. `gatel-state/src/pool/`, `gatel-http/src/proxy/`, `gatel-http/tests/retry.rs` |
| Active health checks, TCP and HTTP | yes | done | `gatel-state/src/supervisor.rs`, `probe.rs` |
| Passive health with ejection | yes | done | `gatel-state/src/endpoint.rs`, `gatel-http/tests/proxy.rs` |
| Safe retry: any failure before a byte was sent, whatever the method | yes | done | `gatel-http/src/proxy/mod.rs`, `gatel-http/tests/retry.rs` |
| Hop-by-hop headers stripped by rebuilding the request | yes | done | `gatel-http/src/proxy.rs` |
| `X-Forwarded-For` from a canonical client address, decided once | yes | done | `gatel-http/tests/proxy.rs` |
| TLS to a backend: SNI, a private authority, a client identity | yes | done | `gatel-tls/src/client.rs`, `gatel-http/tests/proxy.rs` |
| No way to skip verifying a backend's certificate | yes | done | the option does not exist; `tests/proxy.rs` proves an untrusted backend is refused |
| Backends over a Unix domain socket | yes | done | `unix:/run/app.sock`; dialled, pooled and health-checked like any other, and refused at compile time where the platform has none. `gatel-http/tests/unix_socket.rs`, `gatel-state/src/probe.rs` |
| Backends named by host rather than address | yes | done | `gatel-plan/src/upstream/target.rs`, `gatel-state/src/resolve.rs`, `gatel-http/tests/resolve.rs` |
| A name's addresses refreshed without a reload, and without resetting health or pools | yes | done | the name is the endpoint's identity; `gatel-state/src/resolve.rs`, `tests/stability.rs` |
| Retry with a replayed body | yes | done | `retry mode="replay"` buffers a body up to the size the operator named and sends it again after a post-send failure; a body larger than that, or one with no declared length, is streamed and not replayed. `gatel-http/tests/retry.rs` |
| A per-attempt deadline, so one slow endpoint cannot spend the whole budget | yes | done | `per-try`; `gatel-http/tests/retry.rs` |
| A retry budget, so a failing backend is not given twice the load | yes | done | capped as a share of the upstream's live traffic. `gatel-state/src/retry.rs`, `gatel-http/tests/retry.rs` |
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
| `file-server` — range requests, and large files streamed rather than buffered | yes | done | `gatel-bundle-std/src/ranges.rs`, `gatel-http/tests/directives.rs` |
| `file-server` — a fallback, so a single-page application can be hosted at all | yes | done | `fallback=` is nginx's `try_files $uri $uri/ /index.html`; `gatel-http/tests/directives.rs` |
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
| `forward-auth` | yes | done | `gatel-bundle-std/src/forward_auth.rs`, `gatel-http/tests/forward_auth.rs`; asks a declared upstream through the same pool the proxy uses, and one of those tests proves it shares the connection |
| `cache` — in memory, bounded, honouring the response and `Vary` | yes | done | `gatel-bundle-std/src/cache/`, `gatel-http/tests/cache.rs`; no revalidation, no request coalescing, and expired entries are reclaimed on read or eviction rather than by a sweeper — see the module docs |
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
| Three-platform CI | yes | **configured, never run** | `.github/workflows/ci.yml` covers Linux, macOS and Windows — but this branch has never been pushed, so none of it has executed. The suite has been run by hand on Linux in a container, and on Windows; macOS has not been tried at all |
| Fuzzing of the frontend, the compile path, the range parser and the matchers | yes | done | `fuzz/`, four targets, run in CI on every push; found three bugs on its first run |
| The same inputs checked on a stable toolchain, everywhere, every time | yes | done | `gatel-http/tests/robustness.rs`, where a fuzzer's finding becomes a permanent test |
| A soak test | yes | done | `gatel-http/tests/soak.rs`: traffic and reloads in a loop, asserting state slots, metric series, parked connections, log sinks and live tasks are flat. Two minutes per push, half an hour nightly |
| Interoperability suite against curl, browsers and other proxies | yes | partial | tests speak raw HTTP on the wire, and CI runs the container image and talks to it with curl; there is no browser or CDN suite |
| Every event name written down and checked | yes | done | `docs/EVENTS.md`, `cargo run -p xtask` |
| Release matrix and signed artefacts | yes | done | `.github/workflows/release.yml`: eight targets, SBOM, checksums, cosign keyless. Every step of it except pushing and signing also runs on each push, so a tag does not discover it broken |
| A dependency audit that fails the build | yes | done | `deny.toml` and CI: advisories, licences, banned crates and sources |
| A container image | yes | done | `Dockerfile`, `scratch` over a static musl binary; CI builds it and serves a request through it |
| `gatel upgrade` | no | not started | — |

## What must land before 2.0 can be called done

In order of what a deployment would miss first:

1. **ACME against a real certificate authority.** The protocol is `certon`'s
   and the integration is covered end to end against a test authority that
   really fetches the challenge, but nothing here has met Let's Encrypt's
   staging environment. It needs a real domain, which is why it is the one
   remaining item that cannot be closed by writing code.
2. **Interoperability against real clients.** The tests speak raw HTTP and CI
   drives the container image with curl, which is not the same as having been
   through a browser, an HTTP/2 client that multiplexes aggressively, and a
   CDN.
3. **The release workflow has never produced a release.** Every step is
   arranged to run on every push except the two that need a tag — pushing
   artefacts and signing them — so the first tag is a smaller risk than it
   was, but it is not zero.
4. **Nothing has been run by CI.** The branch has never been pushed, so every
   workflow in `.github/workflows/` is configuration that has not executed:
   the three-platform matrix, the fuzzing, the soak, the supply-chain check
   and the packaging job. Pushing it is the cheapest remaining thing that
   would tell us something.

   That is not theoretical. Running the suite on Linux by hand, once, found
   that manual TLS certificates did not work there at all — a certificate
   chain was being refused for being world-readable, which it is supposed to
   be. Everything passed on Windows, where that check is a deliberate no-op.
   macOS is still untried, and is the platform nothing here has ever touched.

## Measured against nginx and Caddy

What both of them have and this does not, in the order a deployment would miss
it:

| | nginx | Caddy | here |
|---|---|---|---|
| `forward-auth` / `auth_request` | yes | yes | yes — the authoriser is a declared upstream, so it gets health checks, pooling and TLS like any other backend |
| Unix-socket *listeners* | yes | yes | **no** — backends are done; listening on a path is not |
| OCSP stapling | yes | yes | **no** |
| HTTP/3 | yes | yes | no |
| Layer-4 TCP/UDP proxying | yes (`stream`) | plugin | no |
| Response body rewriting (`sub_filter`) | yes | plugin | no |
| Variables and `map` | yes | yes | no, deliberately — see `ARCHITECTURE.md` |
| Admin API over HTTP | no | yes | no (2.1) |

And what is here that neither has out of the box, so the comparison runs both
ways: active health checks (nginx Plus only), a reload that is transactional
rather than best-effort, ten balancing strategies, and a compile step that
refuses an ambiguous route instead of letting file order decide it.

`forward-auth` was the largest of these gaps, and it was an architectural one
rather than an unwritten directive: a directive lives in `gatel-bundle-std`,
which may not depend on `gatel-state`, so it could not reach the connection
pool, and giving it a private HTTP client would have meant a second dialling
path with its own timeouts, no pooling and no health — the thing the proxy
exists to avoid.

It was closed by adding the seam rather than the shortcut. `gatel-plan` now
declares what it means to ask an upstream a question about a request
(`Ask`/`Answer`/`Upstreams` in `subrequest.rs`), `gatel-state` implements it on
the runtimes that already own the pools, and `BuildContext` hands a directive
the set of backends its own generation can reach. `forward-auth` is the first
user of it, and `gatel-http/tests/forward_auth.rs` asserts that three
consecutive checks reach the authoriser over one pooled connection — which is
the property the shortcut would have lost.

Everything else marked 2.1 is deliberate: `circuit-breaker`, the other ACME
challenge types and the Admin API are each large enough to want their own
release, and none of them is needed to put this in front of a service.

## Where the ACME protocol comes from

`certon`, which is the same organisation, the same licence, and already what v1
uses — one implementation rather than two. Only its issuer is used, not its
certificate manager: Gatel already owns a certificate store, a resolver, a
background supervisor and a route table, and adopting a second set would split
"a renewal is not a configuration change" across two places that both believe
it.
