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
    balance "weighted-round-robin"
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

**Cosmetic edits do not rebuild the runtime.** Rewriting a file with the same
bytes does not advance its revision. Reformatting or adding a comment can still
parse, normalize and compile, but an unchanged plan skips resource preparation,
activation and generation advancement. External certificate and secret changes
can still require a resource refresh.

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
error[unknown-property]: `balance "round-robin"` has no property `header`
  --> gatel.kdl:14:19
```

## What works today

| | |
|---|---|
| Serving | HTTP/1.1 and HTTP/2, keep-alive, graceful drain, WebSocket and other upgrades, PROXY protocol v1/v2 for the client address behind a layer-4 balancer |
| Routing | host (exact, wildcard, catch-all), path (exact, prefix), method, header/query/cookie/client-ip matchers |
| Reverse proxy | backends by name, by address, or over a Unix socket — a name is the endpoint's identity, and what it resolves to changes under it without resetting health or pools — TLS to backends with a private authority and client identity, 10 balancing strategies, bounded connection pool with reuse, per-stage timeouts, active and passive health, and retry that honours what it says — pre-send failures on any method, replayed bodies when asked and bounded, a per-attempt deadline, and a budget so a failing backend is not handed twice the load |
| TLS | manual PEM certificates, SNI, mutual TLS, automatic certificates over ACME (HTTP-01), and a development authority; eligible TLS-only local sites infer an internal certificate, while `tls internal` selects one explicitly. Client trust is a separate, platform-dependent setup step |
| Reload | file watching with a polling fallback, transactional commit, last-known-good, no-op detection, listener add/remove without restart |
| Directives | `respond`, `redirect`, `file-server` (streaming, ranges, conditional requests, fresh precompressed sidecars, opt-in directory browsing, single-page-app fallback), `cache`, `rewrite`, `trailing-slash`, `force-https`, `headers`, `encode`, `cors`, `access-log`, `request-id`, `basic-auth`, `ip-filter`, `rate-limit`, `forward-auth`, `timeout`, `body-limit`, `error-pages`, `metrics`, `health` |
| Optional capabilities | source-linked CGI, FastCGI, SCGI, maintenance mode, request decompression, and Argon2id authentication; selected at build time, not loaded as dynamic libraries |
| Observability | Prometheus metrics whose label set is the size of the configuration, an access log in text/JSON/CLF to stderr or a rolled file, and readiness that distinguishes what is wrong from what a restart would fix |
| Limits | request head and body size, TLS handshake and idle-connection timeouts, and a process-wide connection cap — each applied, not just declared |
| Tooling | `run`, `serve`, `fmt` (stdout / CI check), `validate`, `explain`, `diff`, `capabilities`, `passwd`, `precompress`, `completions`, `man-page`, `trust`, `untrust` |

The optional `admin-api` host feature adds authenticated loopback configuration
PUT with version CAS, version/status queries, and optional checkpoint recovery.
It does not restore the legacy resource API or implement production WAL and
durable idempotent receipts. See [ADMIN.md](docs/ADMIN.md).

Not yet: distributed tracing, HTTP/3, and Unix-socket listeners.
What is done, what is missing and what proves each is in
[docs/FEATURES.md](docs/FEATURES.md), and every event it emits is in
[docs/EVENTS.md](docs/EVENTS.md). Where the code came from and what may be
taken from elsewhere is [docs/design/provenance.md](docs/design/provenance.md);
what an attacker can reach is
[docs/design/threat-model.md](docs/design/threat-model.md). No performance
number is claimed anywhere — [benchmarks/http-servers](benchmarks/http-servers)
runs gatel, nginx and Caddy side by side on your machine, which is the only
place a number means anything.

Plugin contracts and limitations are in [PLUGINS.md](docs/PLUGINS.md), with
operator guides for [CGI/FastCGI/SCGI](docs/CGI.md),
[request decompression](docs/DECODE.md), [authentication](docs/AUTH.md),
[CLI tools](docs/CLI.md), and [precompression](docs/PRECOMPRESS.md).
See [CERTON.md](docs/CERTON.md) for the certon 0.3 integration boundary and
[compat-v1.md](docs/compat-v1.md) for migration gaps. The ongoing implementation
plan is [ROADMAP.md](docs/ROADMAP.md); this branch does not yet restore all v1 features.

## Building

Requires Rust 1.94 or newer (2024 edition). Use the committed lockfile for
reproducible dependency resolution. Optional plugins are checked together with
`cargo check --workspace --all-features --all-targets --locked`; this is also
the minimum-supported-Rust-version compilation gate.

```sh
cargo build -p gatel --release --locked
cargo test --workspace --locked
cargo run -p xtask --locked # dependency layering and file size checks
```

The default host includes the standard directives but none of the optional
capabilities. For example, build a host with FastCGI and request decompression:

```sh
cargo build -p gatel --release --locked --features fastcgi,decode
./target/release/gatel capabilities
```

Available host features are `maintenance`, `fastcgi`, `scgi`, `cgi`, `decode`,
`argon2`, and `admin-api`. `capabilities` lists the directives in the actual binary;
`passwd --help` lists its available password algorithms. For the full feature
test suite, use `cargo test --workspace --all-features --all-targets --locked`.
Explicitly ignored interoperability and soak tests require separate execution.

## Layout

```
crates/
  gatel-model        the format-neutral configuration tree, diagnostics, identity
  gatel-macros       #[derive(FromNode)]: a struct becomes a strict decoder
  gatel-kdl          the KDL frontend — the only crate that knows KDL exists
  gatel-observe      metrics and health — below everything that can open a socket
  gatel-plan         normalize, compile, and the configuration transaction
  gatel-plugin-api   format-neutral plugin capabilities and effect metadata
  gatel-plugin-runtime runtime bridge for source-linked plugins
  gatel-state        endpoints, health, balancing, connection pools
  gatel-http         listeners, routing, the reverse proxy terminal
  gatel-tls          TLS transport, the certificate store, the development CA
  gatel-bundle-std   the standard directives
  gatel-password    bounded password-hash policy and verification
  gatel-cgi-protocol shared CGI-family protocol types
  gatel-cgi-http    shared CGI-family HTTP adaptation and admission
  gatel-process-windows Windows child-process support
  gatel              the command line
plugins/             optional maintenance, fastcgi, scgi, cgi, and decode packages
xtask/               checks cargo cannot express
```

This list is a package map, not a dependency order. `cargo run -p xtask`
enforces the explicit dependency allowlist in `xtask/src/layers.rs`, including
optional and platform-specific dependencies. The compiler layer has no
network-runtime dependency; adding one requires changing that checked boundary.

## License

Apache-2.0. See [LICENSE](LICENSE).
