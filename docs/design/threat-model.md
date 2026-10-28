# Threat model

What an attacker can reach, what stops them, and which of those defences is
actually tested. A row with no test is a belief, and is marked as one.

The gateway sits between the internet and something the operator cares about. It
is therefore assumed that **every byte of every request is chosen by an
adversary**, that a backend may be compromised, and that a configuration file
may be edited by someone who is careless rather than hostile — the last one
matters more often than the first two.

## What is trusted

- The operator's configuration, and the files it names. Someone who can edit it
  can already do anything the process can.
- The machine's filesystem and clock.
- The Rust toolchain and the dependency tree. Main-workspace production targets
  and build tooling forbid unsafe Rust except for the explicit Windows Job Object
  boundary below. That boundary uses owned handles and checks its FFI contracts;
  neither safe Rust nor this policy makes dependencies memory-safe by itself.

Unsafe production boundary: `gatel-process-windows`.

The exception is [job.rs](../../crates/gatel-process-windows/src/job.rs), with
`deny(unsafe_op_in_unsafe_fn)` at its crate root. Integration fixtures such as
[secrets.rs](../../crates/gatel-http/tests/secrets.rs) may use unsafe environment
mutation; they are not part of the shipped runtime. `cargo run -p xtask --locked
-- contracts` checks production entrypoints and this allowlist.

## What is not trusted

- Request lines, headers, bodies, paths and query strings.
- The peer address, unless it is a configured trusted proxy.
- Anything a backend sends back.
- Anything a certificate authority sends back.

## Surfaces

### Configuration

| Threat | What stops it | Tested by |
|---|---|---|
| A typo silently changing behaviour | Unknown node, property or reference is an error with a position; no silent fallback anywhere | [gatel-plan/tests/normalize.rs](../../crates/gatel-plan/tests/normalize.rs) |
| An `import` reaching outside the configuration tree | `..` is refused; globs only in the last component; cycles detected | [KDL load tests](../../crates/gatel-kdl/src/load.rs) |
| A secret ending up in version control | `env:` and `file:` references only; `basic-auth` refuses an inline hash; no example may contain one | [gatel-model/src/secret.rs](../../crates/gatel-model/src/secret.rs), [gatel-http/tests/examples.rs](../../crates/gatel-http/tests/examples.rs) |
| A secret leaking into a log or a diagnostic | A compiled plan holds the reference, never the material; material exists only inside a built handler, which nothing renders; an error raised while holding it does not quote it | [gatel-http/tests/secrets.rs](../../crates/gatel-http/tests/secrets.rs) (verified to fail when a leak is introduced) |
| A world-readable key file | Bounded `file:` secret reads and Unix permission checks; Windows private directory ACLs remain the operator's responsibility | [gatel-model/src/secret.rs](../../crates/gatel-model/src/secret.rs) |
| A bad edit taking the gateway down | The transaction: rejected, prepare-failed and stale all leave the running generation untouched | [gatel-http/tests/reload.rs](../../crates/gatel-http/tests/reload.rs) |
| A failed edit changing a running quota or log writer policy | Log rotation declarations are staged and published only at commit; conflicting policies for one canonical path are refused. Changing rate-limit `max-keys` rebuilds its state and resets counts; diff reports the binding as restarted for an incompatible change. Unchanged compatible counts remain live after a failed candidate. Preparing a new log destination can still create/open its file | [policy_reload.rs](../../crates/gatel-http/tests/policy_reload.rs), [sink policy tests](../../crates/gatel-observe/src/sink_policy_tests.rs) |
| A deeply nested file exhausting the stack | Depth is bounded before the parser sees the text. A stack overflow is not a panic — nothing catches it, and the process dies, so the check has to come first | [gatel-kdl/src/lower.rs](../../crates/gatel-kdl/src/lower.rs), [gatel-http/tests/robustness.rs](../../crates/gatel-http/tests/robustness.rs) |

### Request handling

| Threat | What stops it | Tested by |
|---|---|---|
| Path traversal out of a served directory | Containment is checked on the *resolved* path, after symlinks are followed — a check on the request text is defeated by a link inside the root | [gatel-bundle-std/src/file_server.rs](../../crates/gatel-bundle-std/src/file_server.rs) |
| Public aliases exposing protected or hidden files within the root | Routing normalizes URL escaping, separators and dot components before guards. File policy is also checked on the resolved path for direct files, listings, indexes and compressed variants. Windows additionally requires the exact canonical relative spelling, rejecting case/short-name/link aliases; Unix links to public internal files remain allowed. Concurrent filesystem mutation and hard-link aliases remain outside this boundary | [gatel-plan/src/request_path.rs](../../crates/gatel-plan/src/request_path.rs), [gatel-bundle-std/src/file_path.rs](../../crates/gatel-bundle-std/src/file_path.rs), [gatel-http/tests/routing_paths.rs](../../crates/gatel-http/tests/routing_paths.rs), [gatel-http/tests/file_listing.rs](../../crates/gatel-http/tests/file_listing.rs) |
| Slowloris | `idle` bounds automatic protocol detection and HTTP/1 or HTTP/2 connections without an active complete request, including partial heads. Handler execution and streaming responses remain active; incomplete extra H2 headers while another stream is active do not have an independent header deadline | [gatel-http/tests/limits.rs](../../crates/gatel-http/tests/limits.rs), [gatel-http/tests/connection_limits.rs](../../crates/gatel-http/tests/connection_limits.rs) |
| A head large enough to exhaust memory | `header` bounds hyper's read buffer and the HTTP/2 header list | [gatel-http/tests/limits.rs](../../crates/gatel-http/tests/limits.rs) |
| A body large enough to exhaust memory or disk | `body` refuses a declared length before reading, and cuts off an undeclared one mid-stream | [gatel-http/tests/limits.rs](../../crates/gatel-http/tests/limits.rs), [body limit tests](../../crates/gatel-bundle-std/src/body_limit.rs) |
| A large file exhausting memory as it is served | Anything past a small threshold is streamed off disk | [gatel-bundle-std/src/ranges.rs](../../crates/gatel-bundle-std/src/ranges.rs) |
| A range that reads past the end of a file | Every branch either lands inside the file or refuses; checked over mutated input for five file sizes | [gatel-bundle-std/src/ranges.rs](../../crates/gatel-bundle-std/src/ranges.rs), [gatel-http/tests/robustness.rs](../../crates/gatel-http/tests/robustness.rs) |
| A resumed download splicing two different files together | A present-but-unreadable `If-Range` counts as not matching, not as absent | [gatel-bundle-std/src/ranges.rs](../../crates/gatel-bundle-std/src/ranges.rs) |
| Connection exhaustion | `connections` limits admitted connections process-wide and shrinks as older connections exit. Each listener may hold one accepted socket waiting for admission, without a task or protocol handshake; further connections remain in its backlog. Idle listeners reserve no permit. The admitted cap is not a hard bound on all process file descriptors | [gatel-http/tests/connection_limits.rs](../../crates/gatel-http/tests/connection_limits.rs) |
| A forgotten WebSocket holding a slot for ever | Idle and total-lifetime watchdogs on every tunnel | [gatel-http/src/upgrade.rs](../../crates/gatel-http/src/upgrade.rs) |
| Spoofing a client address to defeat an IP rule | A forwarding header is believed only from a configured `trusted-proxy`; the canonical address is decided once, where that policy lives | [gatel-http/tests/proxy.rs](../../crates/gatel-http/tests/proxy.rs) |
| Spoofing a client address through a PROXY header | Read only on a listener told to expect one, only from a `trusted-proxy`, and a connection from anywhere else is closed. A listener with no `trusted-proxy` does not compile | [gatel-http/tests/proxy_protocol.rs](../../crates/gatel-http/tests/proxy_protocol.rs), [gatel-plan/tests/normalize.rs](../../crates/gatel-plan/tests/normalize.rs) |
| Request smuggling via hop-by-hop headers | The outbound request is rebuilt rather than edited, so nothing survives that was not deliberately copied | [gatel-http/src/proxy.rs](../../crates/gatel-http/src/proxy/mod.rs) |
| A rewrite slipping past a guard | `rewrite` is in the request phase, after every guard, and position in the file cannot move it | [gatel-bundle-std/src/rewrite.rs](../../crates/gatel-bundle-std/src/rewrite.rs) |
| A cached response reaching somebody a guard would have refused | `cache` is a request-phase directive, so every guard runs on a hit exactly as on a miss | [gatel-http/tests/cache.rs](../../crates/gatel-http/tests/cache.rs) |
| One person's response served to another | A response that sets a cookie, says `private`, or answers an `Authorization` without saying `public` is never stored; `Vary` is part of the key, and `Vary: *` is not stored at all | [gatel-bundle-std/src/cache/policy.rs](../../crates/gatel-bundle-std/src/cache/policy.rs) |
| A metrics or health endpoint exposed by accident | They are ordinary routes on an operator-selected listener and use its guards. The separate optional admin API must be explicitly enabled on authenticated loopback; it is not an automatic metrics port | [gatel-http/tests/observe.rs](../../crates/gatel-http/tests/observe.rs), [admin interface](../ADMIN.md) |
| A configuration-only check reporting a stalled runtime as healthy | `probe` connects to a configured local health route, checks its health-terminal marker and status, and bounds connect/write/header time and size. A generic catch-all 200 is not accepted | [probe implementation](../../crates/gatel/src/probe.rs), [probe regressions](../../crates/gatel/tests/probe.rs) |
| A cache miss retaining an unbounded response | Known oversized responses skip collection; unknown bodies stop collection at the entry bound and replay the prefix. Shared per-store fill reservations and a collector limit bound unfinished payload separately from retained entries; transport buffers and RSS are outside this logical budget | [fill policy](../../crates/gatel-bundle-std/src/cache/fill.rs), [streaming bounds](../../crates/gatel-http/tests/cache_fill_bounds.rs), [fill cancellation](../../crates/gatel-http/tests/cache_fill_budget.rs) |
| High-cardinality labels exhausting memory | A metric series can only be created from a name in the plan; the request path holds a resolved handle with no way back to the registry | [gatel-http/tests/observe.rs](../../crates/gatel-http/tests/observe.rs) |
| Log disk failures silently discarding records | Separate write, flush and rotation error counters, last successful write time and current-fault state expose failures per configured sink. Readiness degrades with `log_io_failed` without blocking requests or removing an otherwise serving instance. The writer emits independent, rate-limited stderr diagnostics; a successful write is not a power-loss durability receipt | [sink I/O status](../../crates/gatel-observe/src/sink_io.rs), [fault injection tests](../../crates/gatel-observe/src/sink_policy_tests.rs), [health state](../../crates/gatel-observe/src/health.rs) |
| A stalled log writer blocking runtime destruction or accumulating on repeated reloads | Sink destructors only close queues. A registry owns at most 128 active/retiring writers, refuses reopening a path until retirement completes, and offers asynchronous drain. The host allows two additional seconds after request/plugin grace; timeout is counted without synchronous diagnostic I/O. OS writes cannot be forcibly interrupted | [sink lifecycle tests](../../crates/gatel-observe/src/sink_lifecycle_tests.rs), [host shutdown regression](../../crates/gatel/src/app_log_shutdown_tests.rs), [metrics and limits](../METRICS.md) |
| A crafted path reading the challenge store | The token alphabet is checked before any lookup | [gatel-http/tests/acme.rs](../../crates/gatel-http/tests/acme.rs) |

### TLS

| Threat | What stops it | Tested by |
|---|---|---|
| Downgrade to plaintext | A TLS listener has no plaintext path; `force-https` redirects rather than serves | [gatel-http/tests/guards.rs](../../crates/gatel-http/tests/guards.rs) |
| A backend impersonated because verification was off | There is no way to turn it off, in either direction | [gatel-http/tests/proxy.rs](../../crates/gatel-http/tests/proxy.rs) |
| A private authority silently widening trust | A named `ca` replaces the public roots rather than adding to them | [gatel-tls/src/client.rs](../../crates/gatel-tls/src/client.rs) |
| A certificate that will not load breaking traffic | Certificates load during prepare, so a bad one stops a reload | [gatel-tls/src/server.rs](../../crates/gatel-tls/src/server.rs) |
| File-only rotation leaving server identity or inbound client trust stale | Candidate preparation observes the exact manual cert/key and inbound CA bytes it parsed. Ordinary file reload compares private fingerprints and conditionally refreshes unchanged KDL; missing, invalid or inconsistent material leaves the serving generation intact. Cross-file atomic replacement and emergency revocation of established connections are not provided | [server material](../../crates/gatel-tls/src/server_material.rs), [real handshake regressions](../../crates/gatel/src/app_server_tls_tests.rs) |
| A trust anchor installed without the operator deciding | `gatel trust` is its own command and is never run implicitly | by construction |
| An unknown name getting a certificate issued for it | Only names the configuration declares are ordered for; on-demand issuance does not exist | by construction |

### Outbound

| Threat | What stops it | Tested by |
|---|---|---|
| Server-side request forgery through a backend address | Backends come from the configuration only; nothing in a request chooses one | by construction |
| Unexpected egress during certificate issuance | The operator chooses the ACME directory; the certon transport performs the protocol. This is not a general network egress sandbox | [gatel-http/tests/acme.rs](../../crates/gatel-http/tests/acme.rs) covers configured issuance, not every hostile CA response or destination |
| A hostile backend exhausting the gateway | Per-stage timeouts, a bounded pool, passive ejection, and a route-level `timeout` | [pool tests](../../crates/gatel-state/src/pool/tests.rs), [gatel-http/tests/guards.rs](../../crates/gatel-http/tests/guards.rs) |
| An H1 pool retaining old capacity or idle policy after a shrink | Returned permits consume shrink debt before new admission; lease return and publication share the current idle/lifetime policy. Failed prepares and NoOp preserve the active policy; already admitted work is not cancelled | [resize and return tests](../../crates/gatel-state/src/pool/resize_tests.rs), [publication regression](../../crates/gatel-http/tests/upstream_pool_reload.rs) |

## Known gaps

Stated rather than hidden, because a threat model that only lists wins is
marketing.

- **The secret tests cover the paths that exist, not every path that could.**
  They drive real material through prepare and through everything a plan can
  print. A new rendering added somewhere else would not automatically be
  covered.
- **Fuzzing is shallow.** The CI inventory below runs a short per-target search.
  That detects regressions but does not establish long-run coverage. Production
  load, filesystem durability and whole-process memory use are not parser
  invariants. See the [fuzz scope and smoke evidence](../../fuzz/README.md).
- **`cargo udeps` is not wired up.** `cargo deny` is, and gates advisories,
  licences, banned crates and sources — but a dependency that is declared and
  never used still ships.
- **The soak test measures what the gateway can count**, not resident memory:
  state slots, metric series, parked connections, log sinks and live tasks. An
  allocation that nothing tracks would not show up in it. Half an hour
  nightly, which is not a day.
- **Request decoding is optional and bounded.** A host built with `decode`
  validates supported encoded input before forwarding the decoded body, with
  encoded/decoded size, expansion, concurrency and elapsed-time limits. Without
  that capability it is not a transparent inspection layer. Decoder working
  memory is not an exact RSS budget, and an individual synchronous codec call
  is not preempted by the async timeout. The [decode guide](../DECODE.md),
  [HTTP regressions](../../crates/gatel-http/tests/decode.rs) and
  [decoder tests](../../plugins/decode/src/lib.rs) describe the tested profile.


## Checked build inventory

CI fuzz targets: `cgi_protocol`, `configuration`, `kdl_frontend`, `matchers`, `password_policy`, `range_header`, `request_decode`, `snapshot_codec`.

CI fuzz budget: `180` seconds per target.

These inventories are compared with the [Cargo targets](../../fuzz/Cargo.toml)
and [CI matrix](../../.github/workflows/ci.yml) by
[xtask contracts](../../xtask/src/contracts.rs). Local evidence links are also
checked for existence. This catches stale inventories and moved files; the
meaning of a claimed guarantee still needs code and test review.

## Failure injection coverage

| Scenario already exercised | Evidence and scope |
|---|---|
| Backend accepts without answering; backend stalls between body chunks | [retry.rs](../../crates/gatel-http/tests/retry.rs) checks deadlines and retry policy; it is not a throughput benchmark |
| Backend closes mid-body during cache replacement | [cache_replacement.rs](../../crates/gatel-http/tests/cache_replacement.rs) refuses truncated replacement data and retires the observed copy |
| Client sends no complete head, partial protocol preface, or unfinished initial H2 headers | [limits.rs](../../crates/gatel-http/tests/limits.rs), [connection_limits.rs](../../crates/gatel-http/tests/connection_limits.rs); no independent header deadline for an extra H2 stream while another is active |
| Request body exceeds a declared or streaming limit | [limits.rs](../../crates/gatel-http/tests/limits.rs), [body_limit.rs](../../crates/gatel-bundle-std/src/body_limit.rs); not an exact total-memory proof |
| Failed, cancelled and stale configuration preparations | [reload.rs](../../crates/gatel-http/tests/reload.rs), [bind_publication.rs](../../crates/gatel-http/tests/bind_publication.rs), including unpublished listener cleanup |
| Lost ACME registration response and CA refusal | [acme.rs](../../crates/gatel-http/tests/acme.rs); configured fixtures do not cover all malformed or slow CA protocol responses |
| Mutated configuration, ranges and matchers | [robustness.rs](../../crates/gatel-http/tests/robustness.rs), plus the checked fuzz inventory |

Remaining gaps include systematic mid-read filesystem errors, arbitrary partial
configuration-file reads during replacement, oversized hostile backend headers,
and broad malformed/slow CA response matrices. A successful source snapshot or
a named test file is not evidence of all those failure modes. Longer fuzz and
soak runs, actual RSS/FD measurements and storage-failure validation remain
separate from the short default checks.
