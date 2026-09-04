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
| `gatel-observe` | everything but the model — it is written to from the data plane, so it must sit under all of it |
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

| Change | Endpoints | Pool | Selector | Rate-limit buckets |
|---|---|---|---|---|
| Raise `max-per-endpoint` | reused | **reused** | reused | reused |
| Replace the backend list | replaced | replaced | replaced | reused |
| Change `balance` | reused | reused | **replaced** | reused |
| Raise `rate-limit max` | reused | reused | reused | **reused** |
| Change `rate-limit key` | reused | reused | reused | **replaced** |
| Edit an unrelated route | reused | reused | reused | reused |

Raising a pool limit must not drop live connections; changing the balancing
strategy must not pretend the old cursor still means anything; raising a rate
limit must not hand every client a fresh allowance. `gatel diff` prints this
table for two real configurations before either is applied, and
`gatel-http/tests/stability.rs` fails if any cell of it stops being true — the
table is a test, not a description.

Directives take part in the same table. A directive declares what it needs to
keep between requests, and its state is owned by the site or route it was
written on — so moving it gives it different state, and editing an unrelated
part of the file does not. There is no path to a global singleton.

Certificates are in the same table for the same reason, with one difference: a
renewal is not a configuration change, so it never produces a generation. The
resolver reads through the store on each handshake, which is what lets a
certificate be replaced while established connections carry on.

## Ordering is decided once

Middleware runs in phase order, and the phase is part of a directive's identity
rather than something configuration chooses:

| Phase | Position | Examples |
|---|---|---|
| `observe` | outermost | `access-log` — may fail open |
| `guard` | | authentication, filters, limits, CORS, timeouts — where a request is refused or answered instead of served |
| `request` | | rewriting |
| `response` | innermost | header and body transformation, error pages |

Within a phase: scope depth (site before route), then `order`, then name.
Declaration position appears nowhere. Writing `basic-auth` after `proxy` in the
file cannot move authentication after the terminal.

`order` exists for the few places where alphabetical order would be wrong.
`timeout` carries a negative one so it wraps the other guards rather than
sitting among them: "this route has thirty seconds" means the whole route,
including a check that has to ask something else. Configuration can set `order`
on any directive, which is the escape hatch for a case the defaults did not
anticipate.

A site's own directives also form a fallback chain, run when a request reached
that site and none of its routes matched. Otherwise a site-wide access log would
miss every request to an unknown path — the ones most worth seeing — and a
site-wide error page could not cover its own 404. A request whose host no site
claims runs nothing, because no site's directives apply to it.

Routes are sorted by specificity at compile time — exact host over wildcard over
catch-all, exact path over longest prefix — so the request path walks a fixed
list and takes the first match. Two routes that would claim the same request are
a compile error, because otherwise the winner would depend on declaration order.

## What the process says about itself

Metrics and health are served by ordinary routes — `metrics` and `health` are
directives like any other. There is no admin port, which means there is no
second surface serving privileged information on every interface because
somebody left a default alone. A scrape route can be put on a loopback listener,
behind `ip-filter` or `basic-auth`, at a path nobody guesses, and the rules that
apply to it are the ones written next to it.

**A series is created from a name in the plan, never from a request.** A route,
an upstream and a listener each resolve their instruments during prepare and
hold them for the life of the generation. What the request path has is that
handle, with no way back to the registry — so a path, a header, a client address
or an invented status code cannot become a label, however much traffic arrives.
Status is reported by class rather than by code for the same reason. After a
commit, series the new plan no longer names are dropped, and ones it still names
keep their values: a counter that restarted at zero on every reload would make
every rate wrong precisely when someone is watching a deploy.

A request is counted where its **body ends**, not where the handler returned —
otherwise a streaming download reads as instant, a cancelled one as a success,
and a compressed body at its uncompressed size. The access log records the same
moment for the same reason, in `text`, `json` or `clf`.

An access log is not a diagnostic log, so it has its own destination — standard
error, or a file that is opened during prepare and rolled by size. An open file
is a process resource like a socket: two generations that log to the same path
share one appender, so they cannot interleave half-written lines or rotate out
from under each other. Writing happens on its own thread behind a bounded queue,
and a full queue drops lines and counts them, because nothing on the request
path may wait for a disk.

Readiness distinguishes two kinds of problem, because they call for opposite
responses:

| Condition | Readiness | Why |
|---|---|---|
| Nothing committed yet, no listener bound, draining | **not ready** | Local to this instance. Another one is fine; move the traffic. |
| Reloads failing while the last good configuration serves | **degraded**, still 200 | Every replica reads the same file. Failing readiness here turns one bad edit into a total outage. |
| Every endpoint of an upstream unusable | **degraded**, still 200 | Shared by every replica. Removing this one moves traffic to another that cannot reach the backend either. |

Both kinds are named in the body; only the first takes the instance out of
rotation. Liveness answers yes as long as it can answer at all — a liveness
probe that fails on a downstream condition restarts a process that was working,
usually across every replica at once.

## Whose error is it

`error-pages` replaces the body of an error the gateway produced, and leaves
alone one a backend answered with. A 404 from a backend is very often a JSON
document an application is waiting for, and replacing it with a page would break
the client to make the browser look nicer.

Telling them apart needs a fact only the gateway has, so every refusal it makes
marks the response as its own: a route that matched nothing, a guard that said
no, a backend that could not be reached, a body over the limit. A directive that
forgets the mark costs a custom page, not correctness. `all=#true` covers
everything for the cases where that is genuinely wanted.

## Limits that bite

Every field in `global { limits ... }` maps to something a connection actually
experiences, because a bound that is written down and not applied is worse than
one that was never offered.

| | What it bounds | How |
|---|---|---|
| `header` | the request head | hyper's read buffer for HTTP/1, `max_header_list_size` for HTTP/2 |
| `body` | the request body | refused before reading when the length is declared, cut off mid-stream when it is not |
| `handshake-timeout` | a TLS handshake | a deadline around the accept |
| `idle` | a connection with no complete head | hyper's header read timeout |
| `connections` | the whole process | a permit taken before `accept`, held for the connection |

`idle` covers two things with one number on purpose. A keep-alive connection
waiting for its next request and a client dribbling a head out one byte at a
time are the same thing seen from the socket, and separating them would mean
knowing which the peer intends.

The connection cap is taken *before* accepting rather than after. A connection
accepted and then closed has already cost a handshake and told the client it was
welcome; one left in the backlog costs nothing and lets a load balancer notice.
Lowering the cap never closes anything that is already open — it takes effect as
connections end, because dropping a live request to satisfy a number somebody
just typed is a worse surprise than being briefly over it.

## Why `proxy` is in the kernel

Endpoint selection, health, connection reuse and retry are one correctness
chain. A plugin able to replace any single link could break the others — a
balancer that could pick an ejected endpoint, or a pool that could resurrect a
draining one. Directives contribute middleware and terminals that need nothing
but a request and a response; the proxy needs the whole chain, so it is not a
directive.

## What is deliberately absent

- **No `tls-skip-verify`.** Development uses the built-in CA instead:
  `tls internal` issues short-lived certificates from an authority generated on
  first use, and `gatel trust` installs its root — as its own command, because
  adding a trust anchor changes what every program on the machine believes.
- **No secret material in configuration.** `env:` and `file:` references only,
  and `SecretString` is redacted in every rendering path.
- **No forwarding header believed by default.** `trusted-proxy` decides, and the
  canonical client address is computed once, where that policy lives.
- **No undocumented event.** Every structured event is in `docs/EVENTS.md`, and
  `cargo run -p xtask` fails if the code and the list disagree in either
  direction. An event name is an interface: something is alerting on it.
- **No silent fallback.** An unknown node, property or reference is an error
  with a position and, where possible, a suggestion.
