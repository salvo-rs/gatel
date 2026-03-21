# Contributing to Gatel

Thank you for your interest in contributing to Gatel! This guide will help you get started.

## Development Setup

### Prerequisites

- [Rust](https://rustup.rs/) 1.75 or later (stable)
- Git

### Building

```bash
# Clone the repository
git clone https://github.com/salvo-rs/gatel.git
cd gatel

# Build in debug mode
cargo build

# Build with all features
cargo build --features "http3,bcrypt"

# Run tests
cargo test --workspace

# Run lints
cargo clippy --workspace --all-targets -- -D warnings

# Check formatting
cargo +nightly fmt --all -- --check
```

### Running

```bash
# Quick static file server (no config needed)
cargo run -- serve --port 8080

# Run with a config file
cargo run -- run --config gatel.kdl

# Validate a config without starting
cargo run -- validate --config gatel.kdl
```

## Project Structure

```
crates/
├── gatel/          # Binary — CLI, signal handling, tracing init
├── core/           # Library — all server logic
│   └── src/
│       ├── config/     # KDL configuration parsing
│       ├── hoops/      # Middleware (rate limit, auth, CORS, etc.)
│       ├── goals/      # Terminal handlers (file server, redirect)
│       ├── proxy/      # Reverse proxy, FastCGI, SCGI, CGI
│       ├── router/     # Path matching and request matchers
│       ├── server/     # HTTP/HTTPS/H3 listeners
│       ├── tls/        # TLS and ACME integration
│       ├── admin/      # Admin API endpoints
│       └── stream/     # L4 TCP stream proxy
├── passwd/         # Password hashing utility
└── precompress/    # Static asset precompression
```

## Making Changes

1. **Fork** the repository and create a feature branch from `main`.
2. **Write code** — keep changes focused; one PR per concern.
3. **Add tests** for new functionality where practical.
4. **Run the full check suite** before submitting:
   ```bash
   cargo +nightly fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
5. **Open a pull request** against `main` with a clear description.

### Commit Messages

Use clear, concise commit messages in imperative form:

```
Add CORS middleware support
Fix health check timeout handling
Refactor proxy upstream selection
```

### Code Style

- Follow `rustfmt` defaults (see `rustfmt.toml`).
- Prefer explicit types over excessive `impl Trait` in public APIs.
- Keep modules focused — one responsibility per file.
- Use `tracing` for logging, not `println!` or `eprintln!`.

## Adding a New Middleware (Hoop)

1. Create `crates/core/src/hoops/your_hoop.rs` implementing `salvo::Handler`.
2. Add a variant to `HoopConfig` in `crates/core/src/config/types.rs`.
3. Add a parsing function in `crates/core/src/config/parse.rs`.
4. Register it in `is_known_directive()` in `parse.rs`.
5. Wire it up in `build_route_router()` in `crates/core/src/salvo_service.rs`.
6. Add an example configuration in `examples/`.

## Reporting Issues

- Search existing issues before opening a new one.
- Include your OS, Rust version, and a minimal reproducing config.
- For security issues, see [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
