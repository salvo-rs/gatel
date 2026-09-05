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
- The Rust toolchain and the dependency tree. `#![forbid(unsafe_code)]` is on
  every crate, so memory-safety bugs would have to come from a dependency.

## What is not trusted

- Request lines, headers, bodies, paths and query strings.
- The peer address, unless it is a configured trusted proxy.
- Anything a backend sends back.
- Anything a certificate authority sends back.

## Surfaces

### Configuration

| Threat | What stops it | Tested by |
|---|---|---|
| A typo silently changing behaviour | Unknown node, property or reference is an error with a position; no silent fallback anywhere | `gatel-plan/tests/normalize.rs` |
| An `import` reaching outside the configuration tree | `..` is refused; globs only in the last component; cycles detected | `gatel-kdl` load tests |
| A secret ending up in version control | `env:` and `file:` references only; `basic-auth` refuses an inline hash; no example may contain one | `gatel-model/src/secret.rs`, `gatel-http/tests/examples.rs` |
| A secret leaking into a log or a diagnostic | A compiled plan holds the reference, never the material; material exists only inside a built handler, which nothing renders; an error raised while holding it does not quote it | `gatel-http/tests/secrets.rs` (verified to fail when a leak is introduced) |
| A world-readable key file | Size and permission checks when resolving a `file:` secret | `gatel-model/src/secret.rs` |
| A bad edit taking the gateway down | The transaction: rejected, prepare-failed and stale all leave the running generation untouched | `gatel-http/tests/reload.rs` |
| A deeply nested file exhausting the stack | Depth is bounded before the parser sees the text. A stack overflow is not a panic — nothing catches it, and the process dies, so the check has to come first | `gatel-kdl/src/lower.rs`, `gatel-http/tests/robustness.rs` |

### Request handling

| Threat | What stops it | Tested by |
|---|---|---|
| Path traversal out of a served directory | Containment is checked on the *resolved* path, after symlinks are followed — a check on the request text is defeated by a link inside the root | `gatel-bundle-std/src/file_server.rs` |
| Slowloris | `idle` bounds a connection with no complete head, whether it is dribbling one or sitting between requests | `gatel-http/tests/limits.rs` |
| A head large enough to exhaust memory | `header` bounds hyper's read buffer and the HTTP/2 header list | `gatel-http/tests/limits.rs` |
| A body large enough to exhaust memory or disk | `body` refuses a declared length before reading, and cuts off an undeclared one mid-stream | `gatel-http/tests/limits.rs`, `body_limit.rs` |
| A large file exhausting memory as it is served | Anything past a small threshold is streamed off disk | `gatel-bundle-std/src/ranges.rs` |
| A range that reads past the end of a file | Every branch either lands inside the file or refuses; checked over mutated input for five file sizes | `gatel-bundle-std/src/ranges.rs`, `gatel-http/tests/robustness.rs` |
| A resumed download splicing two different files together | A present-but-unreadable `If-Range` counts as not matching, not as absent | `gatel-bundle-std/src/ranges.rs` |
| Connection exhaustion | `connections` is a permit taken before `accept`, so excess stays in the backlog | `gatel-http/tests/limits.rs` |
| A forgotten WebSocket holding a slot for ever | Idle and total-lifetime watchdogs on every tunnel | `gatel-http/src/upgrade.rs` |
| Spoofing a client address to defeat an IP rule | A forwarding header is believed only from a configured `trusted-proxy`; the canonical address is decided once, where that policy lives | `gatel-http/tests/proxy.rs` |
| Request smuggling via hop-by-hop headers | The outbound request is rebuilt rather than edited, so nothing survives that was not deliberately copied | `gatel-http/src/proxy.rs` |
| A rewrite slipping past a guard | `rewrite` is in the request phase, after every guard, and position in the file cannot move it | `gatel-bundle-std/src/rewrite.rs` |
| A metrics or health endpoint exposed by accident | They are ordinary routes on a listener the operator chose, subject to the guards written on them. There is no admin port | `gatel-http/tests/observe.rs` |
| High-cardinality labels exhausting memory | A metric series can only be created from a name in the plan; the request path holds a resolved handle with no way back to the registry | `gatel-http/tests/observe.rs` |
| A crafted path reading the challenge store | The token alphabet is checked before any lookup | `gatel-http/tests/acme.rs` |

### TLS

| Threat | What stops it | Tested by |
|---|---|---|
| Downgrade to plaintext | A TLS listener has no plaintext path; `force-https` redirects rather than serves | `gatel-http/tests/guards.rs` |
| A backend impersonated because verification was off | There is no way to turn it off, in either direction | `gatel-http/tests/proxy.rs` |
| A private authority silently widening trust | A named `ca` replaces the public roots rather than adding to them | `gatel-tls/src/client.rs` |
| A certificate that will not load breaking traffic | Certificates load during prepare, so a bad one stops a reload | `gatel-tls/src/server.rs` |
| A trust anchor installed without the operator deciding | `gatel trust` is its own command and is never run implicitly | by construction |
| An unknown name getting a certificate issued for it | Only names the configuration declares are ordered for; on-demand issuance does not exist | by construction |

### Outbound

| Threat | What stops it | Tested by |
|---|---|---|
| Server-side request forgery through a backend address | Backends come from the configuration only; nothing in a request chooses one | by construction |
| A certificate authority reaching something it should not | The only outbound the CA causes is Gatel fetching its own directory URL, which the configuration names | `gatel-http/tests/acme.rs` |
| A hostile backend exhausting the gateway | Per-stage timeouts, a bounded pool, passive ejection, and a route-level `timeout` | `gatel-state`, `gatel-http/tests/guards.rs` |

## Known gaps

Stated rather than hidden, because a threat model that only lists wins is
marketing.

- **The secret tests cover the paths that exist, not every path that could.**
  They drive real material through prepare and through everything a plan can
  print. A new rendering added somewhere else would not automatically be
  covered.
- **Fuzzing is shallow.** Four targets run for three minutes each on every
  push, which finds regressions rather than deep bugs. What finds deep bugs is
  a long run against a corpus that grows, and nobody has left one going.
- **`cargo udeps` is not wired up.** `cargo deny` is, and gates advisories,
  licences, banned crates and sources — but a dependency that is declared and
  never used still ships.
- **The soak test measures what the gateway can count**, not resident memory:
  state slots, metric series, parked connections, log sinks and live tasks. An
  allocation that nothing tracks would not show up in it. Half an hour
  nightly, which is not a day.
- **`decode` is not implemented**, so a compressed request body is passed
  through rather than inspected — which also means a decompression bomb is the
  backend's problem rather than the gateway's. That is a deliberate position,
  but it is a position rather than a defence.
- **PROXY protocol is not implemented**, so a deployment behind a load balancer
  that speaks it cannot get the real client address that way, and would have to
  use a forwarding header and `trusted-proxy`.

## Failure injection worth having

None of these exists yet. Listed so the shape is agreed before the work:

- a backend that accepts and never answers, that answers slowly, that closes
  mid-body, and that sends a header block larger than the limit;
- a client that opens connections and never sends, sends one byte a second, and
  sends a body larger than it declared;
- a filesystem that returns errors mid-read;
- a certificate authority that answers with 500s, with malformed JSON, and very
  slowly;
- a configuration file that is truncated while being read.

The parsers, at least, are no longer on this list: `fuzz/` covers the
configuration frontend, the whole compile path, the range parser and the
matchers, and `gatel-http/tests/robustness.rs` keeps the same invariants under
mutated input on every run.
