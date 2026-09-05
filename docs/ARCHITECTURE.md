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
declared dependencies and compares them against a whitelist. The same command
enforces three other things the compiler cannot: file length, agreement between
the events the code emits and `docs/EVENTS.md` in both directions, and that
every configuration field the compiler produces is read by something above it —
because a knob that is accepted and quietly ignored sends an operator looking
in the wrong place.

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

The kinds a directive can use are not a list in the kernel. Four of them belong
to the proxy and one to TLS, because those are one correctness chain; every
other kind is named after the directive that asked for it. A bundle from
another crate declares state exactly as the standard one does, and the name
cannot collide: a binding is keyed by owning resource and kind, the owner is
the scope the directive was written on, and a directive may appear at most once
per scope.

Certificates are in the same table for the same reason, with one difference: a
renewal is not a configuration change, so it never produces a generation. The
resolver reads through the store on each handshake, which is what lets a
certificate be replaced while established connections carry on.

That is also why an automatic certificate is never ordered during a reload. An
order takes seconds when everything works and minutes when it does not, and a
reload that waited for one would be a reload a certificate authority's bad
afternoon could stall. A commit publishes the intent — these sites want
certificates — and a background task reconciles against it afterwards, exactly
as a health check does. A site with no certificate yet cannot be served over
TLS, and readiness says so; it does not get a certificate this process signed
itself, because one of those is a certificate that lies.

The ACME protocol is `certon`'s. What is Gatel's is where the challenge is
answered: the listener the configuration already declares, before routing and
before every guard, rather than a second server the library would otherwise
start on port 80. A certificate authority asking a question is not a visitor —
`force-https` must not redirect it and a rate limit must not refuse it.

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

## The one thing that is filled in

Nothing else in this design guesses, so the exception is worth stating exactly.
A site with no `tls` gets a development certificate when **both** of these hold:

- every listener it could use terminates TLS, so without one it would be served
  nowhere at all, and
- its host can only ever be local — `localhost`, `*.localhost`, or a name under
  `.test`, `.local`, `.internal` or `.home.arpa`, or a loopback, private or
  link-local address.

Both halves matter. The first means this only ever turns an error into the one
outcome that could have been meant; it never moves a site that was already
working. The second means it is not a guess: no public authority will issue for
those names, so a certificate from the built-in authority is the only one that
could exist. A public name in the same position is refused at compile time and
told which of the three ways to get a certificate is missing, because nothing
this process could sign would be accepted for it.

`gatel explain` says so on the site line, and `gatel trust` is still a separate
command: a certificate appearing without being asked for is a convenience, and
changing what every program on the machine trusts is not.

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

## What may be tried again

Every retry is a guess that the backend did not act on the request. The
configuration therefore describes *when the failure happened*, not what it was,
because that is the only thing that makes the guess safe:

| | Retried | Why |
|---|---|---|
| Nothing eligible, no connection, queue full or timed out | any method, any body | the request never left, so the backend cannot have seen it — this is `mode="safe"` |
| The send failed, or no response head arrived | only under `mode="replay"`, and only when the body was buffered or absent | the backend may have acted on it, so this can duplicate a request; the configuration refuses to enable it without a `buffer` size, so the cost is stated |
| An upgrade | never | the client committed to one connection the moment it asked |

Two bounds sit on top. `per-try` gives each attempt a slice of the overall
budget, so one endpoint that accepts and then goes quiet cannot consume the
time the next attempt would have used. `budget-percent` caps retries as a
share of the upstream's live traffic, because the moment a backend starts
failing every request in flight fails at once — and a gateway that retried all
of them would answer a partial outage by doubling the load on what is left.
A budget of zero turns retries off rather than rounding up to the small floor
that keeps a quiet gateway able to retry at all.

## Why `proxy` is in the kernel

Endpoint selection, health, connection reuse and retry are one correctness
chain. A plugin able to replace any single link could break the others — a
balancer that could pick an ejected endpoint, or a pool that could resurrect a
draining one. Directives contribute middleware and terminals that need nothing
but a request and a response; the proxy needs the whole chain, so it is not a
directive.

## Asking a backend without becoming one

`proxy` being in the kernel leaves a real problem behind: `forward-auth` has to
reach a backend, and it is a directive. The two shortcuts are both bad. Giving
the directive its own HTTP client means a second dialling path with its own
timeouts, no connection reuse, no health and no TLS configuration — a copy of
the thing the paragraph above exists to prevent. Moving the directive into the
kernel means the kernel grows every time something needs a round trip.

So the seam is a capability, not a client, and it is deliberately narrow:

| | Where | What it says |
|---|---|---|
| `Ask` / `Answer` / `Upstreams` | `gatel-plan/src/subrequest.rs` | *ask one declared upstream one question about this request*, in the vocabulary of a method, a path and headers |
| the implementation | `gatel-state/src/ask.rs` | the same selector, pool, health, timeouts and retry a proxied request uses, because it is the same backend |
| `BuildContext::upstreams` | handed over when a handler is built | so the set a directive can reach is fixed at build time, not reachable from a request |
| `DirectiveConfig::upstreams()` | checked while compiling | so a name with a typo in it is a diagnostic with a position, not a 502 the first time somebody is refused |

A question carries no body and an answer carries at most 64 KiB of one, read to
the end before the call returns. That bound is what keeps this from quietly
becoming a second way to proxy: it is enough for the page an identity service
writes when it refuses somebody — which is the whole point, since a refusal is
useless without its `Location` — and nowhere near enough to stream a response
through. Nothing about a subrequest outlives it, so a directive cannot hold a
connection.

The state model has the same shape and for the same reason: a directive names
what it needs and the kernel decides how it is satisfied. Neither is an
extension point that lets a directive reach past the kernel; both are the
kernel offering something it already owns.

## What is deliberately absent

- **No `tls-skip-verify`.** Development uses the built-in CA instead:
  `tls internal` issues short-lived certificates from an authority generated on
  first use, and `gatel trust` installs its root — as its own command, because
  adding a trust anchor changes what every program on the machine believes.
- **No `tls-skip-verify` for backends either.** An upstream that speaks TLS is
  verified, and a private authority is named with `ca`. The option exists in
  other gateways because it makes a broken deployment work today, and it is how
  deployments stay broken for years.
- **No secret material in configuration.** `env:` and `file:` references only,
  and `SecretString` is redacted in every rendering path.
- **No forwarding header believed by default.** `trusted-proxy` decides, and the
  canonical client address is computed once, where that policy lives.
- **No undocumented event.** Every structured event is in `docs/EVENTS.md`, and
  `cargo run -p xtask` fails if the code and the list disagree in either
  direction. An event name is an interface: something is alerting on it.
- **No silent fallback.** An unknown node, property or reference is an error
  with a position and, where possible, a suggestion.
