# Gatel

A programmable gateway with a configuration you can read and reloads you can trust.

> **Status: v2 in development.** This branch is a ground-up rewrite. It serves
> traffic, proxies, and reloads transactionally, but it is not a supported
> release. See [what works today](#what-works-today).

```kdl
global {
    http ":8080"
}

site "*" {
    access-log

    route "/health" {
        method "GET"
        respond "ok\n"
    }

    route "/api/*" {
        proxy upstream="api"
    }
}

upstream "api" {
    endpoint "10.0.1.1:8080" weight=3
    endpoint "10.0.1.2:8080"
    balance "weighted_round_robin"
    passive-health max-fails=5 window="30s" cooldown="60s"
}
```

```sh
gatel validate gatel.kdl   # is this configuration valid?
gatel explain  gatel.kdl   # what will it actually do?
gatel run      gatel.kdl   # serve it, reloading when the file changes
```

## What makes it different

**A reload either happens completely or not at all.** Configuration goes through
one path — snapshot, spec, plan, generation — and only the last step is
published. A port already in use, a certificate that will not load, a handler
that cannot be built: all of them fail before anything changes, and the previous
configuration keeps serving. There is no window where half the new configuration
is live.

**An unchanged configuration costs nothing.** Rewriting a file with the same
bytes does not advance its revision. Reformatting it, or adding a comment,
produces the same *spec* fingerprint and stops before compiling. Only a change
that would behave differently reaches the point of building anything.

**Declaration order never decides behaviour.** Middleware runs in a fixed order
of phases — observe, guard, request, response — so writing `basic-auth` after
`proxy` cannot move authentication after the terminal. Routes are sorted by
specificity at compile time, so an exact path always beats a prefix regardless
of where each was written. Two routes that would match the same request are a
compile error rather than a race between them.

**State outlives configuration where it should.** Raising a pool's size limit
keeps every live connection. Changing the backend list does not, and says so.
Changing the balancing strategy resets only the balancer's cursor. Raising a
rate limit keeps what each client has already spent; changing what is counted
starts a fresh count. `gatel diff` shows which, before you apply anything.

**Nothing is silently ignored.** An unknown node, an unknown property, a
misspelled strategy, a reference that does not resolve: each is an error with a
stable code, a file position and, where possible, a suggestion.

```
error[unknown-property]: `balance "round_robin"` has no property `header`
  --> gatel.kdl:14:19
```

## What works today

| | |
|---|---|
| Serving | HTTP/1.1 and HTTP/2, keep-alive, graceful drain, WebSocket and other upgrades |
| Routing | host (exact, wildcard, catch-all), path (exact, prefix), method, header/query/cookie/client-ip matchers |
| Reverse proxy | 10 balancing strategies, bounded connection pool with reuse, per-stage timeouts, active and passive health, safe retry |
| TLS | manual PEM certificates, SNI, mutual TLS, and a built-in development authority with `gatel trust` |
| Reload | file watching with a polling fallback, transactional commit, last-known-good, no-op detection, listener add/remove without restart |
| Directives | `respond`, `redirect`, `file-server`, `rewrite`, `headers`, `encode`, `access-log`, `request-id`, `basic-auth`, `ip-filter`, `rate-limit`, `metrics`, `health` |
| Observability | Prometheus metrics whose label set is the size of the configuration, an access log in text/JSON/CLF to stderr or a rolled file, and readiness that distinguishes what is wrong from what a restart would fix |
| Limits | request head and body size, TLS handshake and idle-connection timeouts, and a process-wide connection cap — each applied, not just declared |
| Tooling | `validate`, `explain`, `diff`, `capabilities`, `trust` |

Not yet: ACME, caching, forward authentication, the admin API, tracing,
HTTP/3, and the CGI family. The plan for those is in `docs/`.

## Building

```sh
cargo build --release
cargo test --workspace
cargo run -p xtask          # dependency layering and file size checks
```

## Layout

```
crates/
  gatel-model        the format-neutral configuration tree, diagnostics, identity
  gatel-macros       #[derive(FromNode)]: a struct becomes a strict decoder
  gatel-kdl          the KDL frontend — the only crate that knows KDL exists
  gatel-observe      metrics and health — below everything that can open a socket
  gatel-plan         normalize, compile, and the configuration transaction
  gatel-state        endpoints, health, balancing, connection pools
  gatel-http         listeners, routing, the reverse proxy terminal
  gatel-tls          TLS transport, the certificate store, the development CA
  gatel-bundle-std   the standard directives
  gatel              the command line
xtask/               checks cargo cannot express
```

Each layer may only depend on the ones below it, and `cargo run -p xtask`
enforces it. That is what makes "the compiler cannot open a socket" a fact
rather than a convention.

## License

Apache-2.0. See [LICENSE](LICENSE).
