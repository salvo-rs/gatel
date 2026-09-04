# Architecture

## The one path

```
 Frontends   KDL file        Admin API (later)      Kubernetes (later)
                │                   │                      │
                └──── each lowers into Node, a format-neutral value tree ────┘
                                    │
 Snapshot   immutable: what every source declared, at one moment
                                    │ normalize — resolve, validate, assign identity
 Spec       immutable resource graph, source-agnostic ─────────────► spec_hash
                                    │ compile — order, flatten, bind state
 Plan       immutable execution plan ──────────────────────────────► plan_hash
                                    │ prepare — bind sockets, build handlers
 Generation published atomically; a connection pins one at accept
                                    │
 State      typed, mutable, owned by resources rather than generations
```

Five nouns, two hashes, one counter. Everything else is named after one of them.

### Why two hashes

They answer different questions.

`spec_hash` answers *did the operator ask for something different?* Comments,
whitespace, declaration order and file layout are gone by the time it is
computed, so reformatting a file produces the same value and the reload stops
there.

`plan_hash` answers *would this behave differently?* It includes the build's
capability set, so the same configuration compiled by a binary without a given
bundle produces a different plan — and the second binary can never mistake the
first one's plan for its own.

## What each layer may not do

The layering is enforced by `cargo run -p xtask`, which reads each crate's
declared dependencies and compares them against a whitelist.

| Crate | Cannot reach |
|---|---|
| `gatel-model` | any async runtime, any I/O, KDL, HTTP |
| `gatel-macros` | anything but syn/quote and the model |
| `gatel-kdl` | anything but the model and `kdl` |
| `gatel-plan` | hyper, rustls, `tokio::net` — it may name `salvo_core::Handler`, but it cannot open a socket |
| `gatel-state` | Salvo routing; it owns transport to backends, not request dispatch |

The point is what they *cannot* do. A compiler that cannot open a socket cannot
grow a side entrance into the data plane, however convenient that would be in
the moment.

## The transaction

```
observe  → publish the source revisions, so a slower candidate can tell it was overtaken
normalize→ Spec, or a list of every problem
compile  → Plan; if plan_hash is unchanged, stop here and consume nothing
prepare  → bind sockets (all or nothing), resolve state, build handlers
commit   → under one lock: re-check the revisions and the generation, then publish
```

Only `commit` takes the write lock, and everything expensive happens outside it.
Correctness comes from re-checking, under the lock, the two things the candidate
assumed: that no source moved, and that no other generation was committed.

Four failure modes, each leaving the running configuration exactly as it was:

- **Rejected** — the configuration is invalid. Every problem is reported.
- **PrepareFailed** — a port is in use, a handler will not build. No generation
  is consumed.
- **Stale** — something newer was observed while this candidate compiled. What
  preparation acquired is dropped, which is why preparation must clean up in
  `Drop`.
- **NoOp** — nothing would behave differently. Nothing is built and no state is
  touched.

## Three lifetimes, kept apart

This is what makes a reload invisible to traffic.

**Sockets belong to the process.** A reload that keeps an address keeps the
socket, so there is no window in which a connection is refused. A reload that
adds addresses binds all of them or none.

**Route tables belong to a generation.** Each generation holds its own immutable
handler graph. Building it cannot mutate the running one.

**A connection belongs to the generation that was current when it was
accepted.** It keeps that one until it closes, so an in-flight request never
sees configuration change underneath it, and an HTTP/2 connection's later
streams stay with the configuration their connection started under.

## State that outlives configuration

State is keyed by `(owning resource, kind)` and carries a compatibility
fingerprint. A reload asks "is what I need already running?" and gets a typed
answer.

| Change | Endpoints | Pool | Selector |
|---|---|---|---|
| Raise `max-per-endpoint` | reused | **reused** | reused |
| Replace the backend list | replaced | replaced | replaced |
| Change `balance` | reused | reused | **replaced** |
| Edit an unrelated route | reused | reused | reused |

Raising a pool limit must not drop live connections, and changing the balancing
strategy must not pretend the old cursor still means anything. `gatel diff`
prints this table for two real configurations before either is applied.

## Ordering is decided once

Middleware runs in phase order, and the phase is part of a directive's identity
rather than something configuration chooses:

| Phase | Position | Examples |
|---|---|---|
| `observe` | outermost | `access-log` — may fail open |
| `guard` | | authentication, filters, limits — where rejections happen |
| `request` | | rewriting |
| `response` | innermost | header and body transformation |

Within a phase: explicit `order`, then scope depth (site before route), then
name. Declaration position appears nowhere. Writing `basic-auth` after `proxy`
in the file cannot move authentication after the terminal.

Routes are sorted by specificity at compile time — exact host over wildcard over
catch-all, exact path over longest prefix — so the request path walks a fixed
list and takes the first match. Two routes that would claim the same request are
a compile error, because otherwise the winner would depend on declaration order.

## Why `proxy` is in the kernel

Endpoint selection, health, connection reuse and retry are one correctness
chain. A plugin able to replace any single link could break the others — a
balancer that could pick an ejected endpoint, or a pool that could resurrect a
draining one. Directives contribute middleware and terminals that need nothing
but a request and a response; the proxy needs the whole chain, so it is not a
directive.

## What is deliberately absent

- **No `tls-skip-verify`.** Development uses the built-in CA instead.
- **No secret material in configuration.** `env:` and `file:` references only,
  and `SecretString` is redacted in every rendering path.
- **No forwarding header believed by default.** `trusted-proxy` decides, and the
  canonical client address is computed once, where that policy lives.
- **No silent fallback.** An unknown node, property or reference is an error
  with a position and, where possible, a suggestion.
