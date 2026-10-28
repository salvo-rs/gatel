# Contributing to Gatel

This tree is the v2 rewrite. Its APIs and configuration surface are still under
development; use the checked-in lockfiles when reproducing a result.

## Development setup

Requires Rust 1.94 or later and Git. Choose an external build-cache directory
appropriate for your machine. For example, in PowerShell:

```powershell
$env:CARGO_TARGET_DIR = 'D:/BuildCache/gatel'
cargo metadata --no-deps --format-version 1 --locked
```

Check the returned `target_directory` before compiling. In a WSL workspace whose
build storage must stay on D, verify the `/mnt/d` mount first and point
`CARGO_TARGET_DIR` at `/mnt/d/BuildCache/gatel`. Docker has its own storage:
setting Cargo's directory does not relocate a Docker builder cache.

```sh
cargo build -p gatel --locked
cargo test --workspace --all-targets --locked
cargo test --workspace --all-features --all-targets --locked
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
cargo fmt --all --check
cargo run -p xtask --locked
cargo +1.94.0 check --workspace --all-features --all-targets --locked
```

The last command requires that toolchain to be installed. `xtask` checks
dependency layering, source sizes, events, inert configuration, and the threat
model's build/evidence inventory. `cargo deny check` gates advisories, licenses,
banned crates and sources. See [CI](.github/workflows/ci.yml) for separate plugin,
interop, fuzz, soak and packaging jobs; explicitly ignored tests need their
documented external fixtures and are not covered by a normal successful run.

Stable rustfmt may report that this tree's optional nightly-only formatting
settings are unavailable. Use nightly formatting when changing those settings;
the stable CI formatting check must still pass.

## Running

```sh
cargo run -p gatel --locked -- serve . --port 8080
cargo run -p gatel --locked -- run gatel.kdl
cargo run -p gatel --locked -- validate gatel.kdl
cargo run -p gatel --locked -- capabilities
```

Configuration paths are positional arguments. `validate` compiles without
preparing resources; `run --dry-run` prepares and binds before exiting. Optional
capabilities include `maintenance`, `fastcgi`, `scgi`, `cgi`, `decode`, `argon2`
and `admin-api`, selected through Cargo features. Check
[FEATURES](docs/FEATURES.md), [CLI](docs/CLI.md), and
[plugin contracts](docs/PLUGINS.md) for their limits and platform support.

## Project structure

- `gatel-model`, `gatel-kdl`, `gatel-macros`: format-neutral input, KDL frontend
  and strict derived decoders.
- `gatel-plan`: normalization, route compilation and configuration transactions.
- `gatel-state`, `gatel-http`, `gatel-tls`: persistent runtime state, listeners
  and proxying, and TLS/certificate ownership.
- `gatel-observe`, `gatel-bundle-std`: metrics/logging/health and standard
  directives.
- `gatel-plugin-api`, `gatel-plugin-runtime`, and `plugins/`: plugin metadata,
  runtime bridge and optional source-linked capabilities.
- `gatel-password`, `gatel-cgi-http`, `gatel-cgi-protocol`,
  `gatel-process-windows`: bounded credential handling and shared CGI/platform
  boundaries.
- `crates/gatel`: executable, watcher and optional management/checkpoint host.
- `xtask/`, `fuzz/`, `benchmarks/`: repository gates, separate fuzz workspace and
  reproducible local performance workloads.

The dependency allowlist in [layers.rs](xtask/src/layers.rs) is checked, not just
suggested. The production unsafe exception is documented in the
[threat model](docs/design/threat-model.md); other production entrypoints forbid
unsafe Rust.

## Making changes

Base changes on the development branch for the version being changed. Keep each
PR focused, describe the observable before/after behavior, and include the
validation relevant to that behavior. Do not treat a test count alone as proof
of protocol, security or resource invariants.

For a new directive:

1. Choose its phase, role, scopes and effects using `gatel-plan` contracts.
2. Implement strict decoding and `DirectiveConfig`; reject unsupported input
   rather than silently accepting an inert knob.
3. Register it through the appropriate standard or optional bundle. Follow
   [PLUGINS](docs/PLUGINS.md) for prepared resources and lifecycle ownership.
4. Declare state compatibility/reset semantics and publication behavior. A
   failed or stale candidate must not mutate the active generation's policy.
5. Add configuration examples and the smallest meaningful unit and real
   protocol regressions. Include cancellation, failure and reload cases when
   the directive owns resources or persistent state.
6. Update capability and threat-model evidence where behavior changes, then run
   the relevant tests and repository checks above.

Use `tracing` for runtime events and document new stable events in
[EVENTS](docs/EVENTS.md). CLI artifacts and intentional command output use
stdout/stderr according to their existing contracts. Keep modules focused and
use concise commit and PR titles describing the change.

## Reporting issues

Include the OS, Rust version, enabled features, minimal configuration, expected
and observed behavior, and a bounded reproduction where possible. For security
issues follow [SECURITY](SECURITY.md).

Contributions are licensed under [Apache License 2.0](LICENSE).
