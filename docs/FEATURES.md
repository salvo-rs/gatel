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
| Admin API as a second source under `runtime/` | no (2.1) | partial | Optional `admin-api`: source-bound KDL PUT, full-version CAS, authenticated version/status queries, loopback listener and diagnostic read reserve. `admin_tests.rs`, `admin_server_tests.rs`, `app_source_union_tests.rs` and CLI tests cover the implemented boundary. Legacy resource endpoints, scoped token permissions and full audit remain open; an `admin` KDL block is still not a supported activation mechanism. See [ADMIN.md](ADMIN.md). |
| Runtime checkpoint persistence, recovery and offline compaction | no (2.1) | partial | `checkpoint_run` CLI tests, `app_checkpoint_*` tests and journal checkpoint/compaction crash tests cover dual snapshots, save-failure latching, restart and source-preserving export. Production mode is checkpoint-only, not WAL; idempotent receipts, online rotation and power-loss validation remain open. See [CLI.md](CLI.md). |
| Kubernetes source | no | not started | — |

## Optional plugins

Optional `decode` restores bounded request decompression for gzip/br/zstd/deflate.
See [DECODE.md](DECODE.md) for limits, buffering, metadata changes and reload scope.

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Plugin SDK independent of compiler | yes | partial | `gatel-plugin-api` and `gatel-plugin-runtime`; maintenance package has no plan dependency; stable API and external fixture remain open |
| Middleware before/conflict constraints | yes | done | `gatel-plan/src/directive.rs` composition tests; normalize validates merged chains before prepare |
| Maintenance mode as a removable package | no | done | `gatel/tests/plugins.rs`: feature on/off capabilities and validation; real HTTP GET/HEAD and live reload |
| Declared upstream grants and single-backend handles | yes | done | SDK dispatch allowlist test and `plugin_lifecycle.rs` host-grant checks |
| Managed plugin tasks and typed resource services | yes | partial | `plugin_lifecycle.rs`, `plugin_tasks.rs`: commit-only, dedup, cancellation, LKG, NoOp, pinning; Secret/DNS/dial services and worker supervision remain open |
| FastCGI / SCGI / child-process CGI packages | yes | partial | Optional FastCGI/SCGI and Windows process CGI share HTTP mapping/lifecycle. Unix socket protocol tests and Windows real-process HTTP tests passed; Linux/macOS process CGI remains open. See [CGI.md](CGI.md). |

## CLI artifacts and v1 migration audit

`gatel fmt [CONFIG] [--check]` now provides single-file, comment-preserving KDL
formatting through the same guarded parser as configuration loading. It never
expands imports, compiles plugins or writes configuration files. Windows/Linux
binary tests cover idempotence, check exit codes, unchanged source, missing imports,
unknown plugin syntax, invalid UTF-8/syntax, nesting and size limits. This does not
complete the outstanding v1 adapter, recursive formatter or in-place editing work.

Basic Auth execution and input limits are documented in [AUTH.md](AUTH.md): host-injected blocking work with atomic job/memory admission, cancellation-retained reservations, KDL blocking budgets published with the generation, an embedding API for usage-preserving limit changes, shared `gatel-password` policies, optional Argon2id (`argon2` feature), strict parameter validation and LKG tests. This does not complete AUTH-01's remaining algorithms, rotation/revocation, fairness or Admin API budget management.

The metrics terminal exposes a coherent budget sample and fixed-cardinality admission/completion counters; see [METRICS.md](METRICS.md). These are shared blocking-work metrics, not authentication-success rates or CPU-time measurements.

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Shell completions and roff manuals generated from the actual command tree | yes | done | `gatel/tests/cli_artifacts.rs`: five shell generators, root/subcommand manuals, current options, deterministic output and usage errors; `src/cli_output.rs`: write/flush error policy. Interactive shell installation and man rendering are not fully verified. See [CLI.md](CLI.md). |
| Static-directory `serve` CLI through the normal transaction and HTTP runtime | yes | done | `gatel/tests/serve.rs`: files, HEAD, Range, browse, hidden files, updates, invalid local KDL ignored, dry-run and errors. `src/serve.rs`: typed immutable Snapshot and NoOp. Defaults to loopback; Unix-only signal/path cases, Windows console Ctrl+C not yet verified. |
| Bounded stdin password hash generation (`passwd --stdin`) | yes | partial | `gatel/tests/passwd.rs`: bcrypt and optional Argon2id capability/CLI/actual HTTP agreement. `gatel-password`: shared validation/generation policy. Hidden interactive input, standalone legacy executable and further algorithms remain open. |
| Complete v1 parameter/default/CLI migration inventory | yes | partial | [compat-v1.md](compat-v1.md) inventories old CLI and utility entry points against local main eaf9bde; configuration/parser/examples and binary differential validation remain open. |

## Reload

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| Transactional commit: rejected, prepare-failed and stale all leave the running configuration untouched | yes | done | `gatel-plan/src/generation.rs`, `gatel-http/tests/reload.rs` |
| Last known good: a bad edit keeps serving the previous generation | yes | done | `gatel-http/tests/reload.rs` |
| No-op detection: reformatting a file consumes no generation | yes | done | `gatel-http/tests/reload.rs` |
| Sockets outlive generations; adding addresses is all-or-nothing | yes | done | `gatel-http/src/bind.rs`, `tests/reload.rs` |
| Connections pin their accepted generation, with opt-in bounded retirement | yes | done | Default pinning; global reload-drain gracefully closes old H1/H2 connections and bounds unfinished bodies/handshakes/tunnels. tests/reload_drain.rs covers default/opt-in, NoOp/rejection, in-flight completion and tunnel deadline/permit release. |
| State survives a compatible reload (endpoints, pools, selectors, rate limits, certificates, metrics) | yes | done | `gatel-state/src/registry.rs`, `tests/observe.rs`, `tests/directives.rs` |
| File watching with a polling fallback, and signals | yes | done | `crates/gatel/src/watch.rs` |
| Reload stability matrix across every state kind | yes | done | `gatel-http/tests/stability.rs`, one row per cell of the table in `ARCHITECTURE.md` |

## Serving

| Capability | Required for 2.0 | Status | Proof |
|---|---|---|---|
| HTTP/1.1 and HTTP/2, keep-alive, graceful drain | yes | done | `gatel-http/tests/serving.rs` |
| WebSocket and other upgrades, with idle and lifetime bounds | yes | done | `gatel-http/tests/upgrade.rs` |
| Upgrades retain generation/connection capacity and obey shutdown grace | yes | done | `upgrade.rs`: real echo tunnel across reload, capacity admission, deadline closes socket; `connection_work.rs` one-shot handoff |
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
| A drained pool cannot reopen for a new endpoint or park returning leases | yes | done | `gatel-state/src/pool/tests.rs`; includes withdrawal of every DNS address and accurate waiter/in-use accounting |
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
| `file-server` — containment, symlinks, ETag, conditional requests, precompressed | yes | done | `gatel-bundle-std/src/file_server.rs`, `gatel-http/tests/directives.rs`; `file_listing.rs` verifies public symlink aliases cannot expose hidden resolved targets through direct requests, directory indexes/listings or precompressed variants. No concurrent filesystem mutation sandbox. |
| `file-server` — range requests, and large files streamed rather than buffered | yes | done | `gatel-bundle-std/src/ranges.rs`, `gatel-http/tests/directives.rs` |
| `file-server` — opt-in directory browsing | yes | bounded implementation | `gatel-http/tests/file_listing.rs`; URL-encoded links, root-contained entries, 10,000 scanned entries / 2 MiB HTML per listing; over-limit listings return non-cacheable 503. Pagination remains pending. |
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
| `cache` — in memory, bounded, honouring the response and `Vary` | yes | done | `gatel-bundle-std/src/cache/`, `gatel-http/tests/cache.rs`; expired entries are reclaimed on read or eviction rather than by a sweeper |
| `cache` — bounded in-progress fills | yes | bounded implementation | `cache_fill_bounds.rs`, `cache_fill_budget.rs`: known oversize skips collection; unknown oversize replays the prefix and remaining stream, retaining errors/trailers. Each store has a separate `max-size` fill payload reservation budget and at most 64 collectors; exhaustion streams without caching. Retained data and unfinished fills can each use `max-size`; transport buffers, allocator overhead and RSS are outside this logical bound. |
| `cache` — origin revalidation | yes | partial | `gatel-http/tests/cache_revalidate.rs`: unencoded GET with ETag or Last-Modified, 304 reconstruction and invalid-validator rejection; client-conditional requests, HEAD, encoded representations, request coalescing and stale-on-error remain outside this profile |
| `cache` — ambiguous cached ETag handling | yes | bounded implementation | Fresh entries with duplicate or malformed ETag forward client If-None-Match to the origin instead of deciding 304/body locally; real HTTP coverage in `cache_revalidate.rs`. Unconditional cache hits remain allowed; this is not wholesale validation of every origin header. |
| `cache` — retirement after an unstoreable new 200 | yes | bounded implementation | `cache_replacement.rs`: private/no-store, missing freshness, invalid Age, oversized and truncated Content-Length/chunked bodies retire the observed copy. Per-copy identity preserves concurrent replacements and other Vary copies; removal advances the bounded global invalidation revision. Non-200 responses and bypassed requests are outside this rule; client-cancellation injection remains pending. |
| `cache` — H2 reset during a cold fill | yes | bounded implementation | `cache_h2.rs`: cancelling before downstream response headers closes the incomplete HTTP/1 origin connection within 3 seconds, while the H2 connection remains usable for a subsequent complete fill and hit. This does not prove the warm-copy retirement timing or H1 disconnect path. |
| `cache` — H1 abortive close during a cold fill | yes | bounded implementation | `cache_h1.rs`: SO_LINGER=0 client close releases the incomplete origin within 3 seconds; a new client connection fills and hits a complete response. This is not a FIN/half-close or warm-copy cancellation test. |
| Argon2id, scrypt and PBKDF2 for `basic-auth` | no | partial | Optional Argon2id is implemented and tested; scrypt/PBKDF2 remain open. See AUTH.md. |
| The CGI family | no | partial | Optional FastCGI/SCGI and Windows child-process CGI plugins are implemented; other process platforms and further resource policies remain open. See `CGI.md` and `ROADMAP.md` for validation boundaries. |

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
| No configuration that is accepted and then ignored | yes | done | `cargo run -p xtask` fails on a field the compiler carries and nothing above it reads. Added after finding three: `retry`'s mode, per-try, buffer and budget, `timeouts body-idle`, and the whole `admin` block |
| Every example validated | yes | done | `gatel-http/tests/examples.rs` compiles every one, and refuses a secret written in place |
| Three-platform CI | yes | partial | Run [34286574542](https://github.com/salvo-rs/gatel/actions/runs/34286574542) at 87d6ffe passed Windows/Linux, MSRV, lint, packaging and interoperability; macOS failed on an invalid-UTF-8 filename fixture, fuzz compilation on a nightly deprecation, and bans on unversioned path dependencies. Current fixes require a new remote run; do not infer it from local checks. |
| Fuzz targets and deterministic regressions | yes | partial | CI now includes all eight targets, including snapshot_codec. The recorded remote run failed to compile fuzz targets. Local nightly compilation fixes are not evidence of a sustained fuzz campaign. |
| The same inputs checked on a stable toolchain, everywhere, every time | yes | done | `gatel-http/tests/robustness.rs`, where a fuzzer's finding becomes a permanent test |
| A soak test | yes | done | `gatel-http/tests/soak.rs`: traffic and reloads in a loop, asserting state slots, metric series, parked connections, log sinks and live tasks are flat. Two minutes per push, half an hour nightly |
| Interoperability suite against curl, browsers and other proxies | yes | partial | tests speak raw HTTP on the wire, and CI runs the container image and talks to it with curl; there is no browser or CDN suite |
| Every event name written down and checked | yes | done | `docs/EVENTS.md`, `cargo run -p xtask` |
| Release matrix and signed artefacts | yes | done | `.github/workflows/release.yml`: eight targets, SBOM, checksums, cosign keyless. Every step of it except pushing and signing also runs on each push, so a tag does not discover it broken |
| A dependency audit that fails the build | yes | done | `deny.toml` and CI: advisories, licences, banned crates and sources |
| A container image | yes | done | `Dockerfile`, `scratch` over a static musl binary; CI builds it and serves a request through it |
| A performance comparison that can be repeated | yes | done | `benchmarks/http-servers/`: gatel, nginx and Caddy in containers under `wrk`, five scenarios, report and raw output per run. It builds the same image the release ships, which is how it found that the image did not build at all |
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
4. **A fully passing remote CI run.** The branch has been pushed and run.
   The recorded run above passed several important gates but failed macOS,
   fuzz compilation and the dependency bans check. Re-run the complete matrix
   after the fixes, then retain the result rather than describing workflows as
   either unexecuted or proven merely because their YAML exists.

## Measured against nginx and Caddy

This is a comparison of **features**. For performance there is
`benchmarks/http-servers/`, which runs all three in containers under `wrk` and
writes a report; no number from it is quoted here, because a number from one
machine belongs with the machine it came from.

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
| Admin API over HTTP | no | yes | optional loopback KDL config/version/status API; full legacy resource management remains open |

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
challenge types and the full legacy Admin resource API are each large enough to want their own
release, and none of them is needed to put this in front of a service.

## HTTP compression interoperability

`encode "deflate"` produces an RFC 1950 zlib-wrapped stream, as required by
[RFC 9110 §8.4.1.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-8.4.1.2),
not raw DEFLATE. Its directive behavior version is 5. Tests check the wrapper,
Adler-32 trailer and complete decoding, plus real HTTP responses with caching
and alternating identity/deflate negotiation. This does not restore the old
request-decompression directive by itself; the separate optional `decode` plugin
now provides bounded decode-before-forward behavior (see above).

## Where the ACME protocol comes from

`certon`, which is the same organisation, the same licence, and already what v1
uses — one implementation rather than two. Gatel uses its issuer and, since
certon 0.3, its semantic certificate persistence contract (`CertStore`). Its
certificate manager does not own Gatel's serving lifecycle: Gatel already owns
an active certificate store, a resolver, a
background supervisor and a route table, and adopting a second set would split
"a renewal is not a configuration change" across two places that both believe
it.
See [CERTON.md](CERTON.md) for the exact policy, persistence and publication split.
