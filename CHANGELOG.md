# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- CORS middleware — reuses `salvo-cors` with full KDL config support
- Timeout middleware — reuses `salvo_extra::timeout`
- Request ID middleware — reuses `salvo_extra::request_id` (ULID-based)
- Force HTTPS middleware — reuses `salvo_extra::force_https`
- Trailing slash middleware — reuses `salvo_extra::trailing_slash`
- Docker support — Dockerfile (distroless), Dockerfile.alpine, docker-compose.yml
- Install scripts — `install.sh` (Linux/macOS), `install.ps1` (Windows)
- Makefile with build, install, uninstall, test, lint, fmt, docker targets
- DEB and RPM packaging infrastructure
- GitHub Actions release workflow with multi-platform builds
- Community files — CONTRIBUTING.md, CODE_OF_CONDUCT.md, SECURITY.md
- Cross-compilation support via Cross.toml
- Build optimizations — LTO, binary stripping, single codegen unit

### Changed

- Refactored proxy handlers to include goals module
- Streamlined import statements across modules

## [0.1.0] - 2025-01-01

### Added

- Initial release
- KDL-based configuration with snippets and hot-reload
- Reverse proxy with 10 load-balancing strategies
- Active and passive health checking
- Automatic TLS via ACME (Let's Encrypt, ZeroSSL)
- Manual TLS certificates with per-site overrides
- Mutual TLS (mTLS) client verification
- On-demand TLS for dynamic certificate issuance
- HTTP/1.1, HTTP/2, and HTTP/3 (QUIC) support
- Response compression (Gzip, Brotli, Zstd, Deflate)
- Static file serving with ETag, range requests, directory browsing
- Rate limiting (token bucket, per-IP)
- Basic authentication with bcrypt/argon2/scrypt/pbkdf2
- Forward authentication delegation
- IP filtering (CIDR allow/deny)
- Header manipulation with placeholders
- URI rewriting with regex support
- Response body replacement
- In-memory HTTP response caching
- Server-side HTML templates
- Request body and response body size limits
- FastCGI, SCGI, and CGI protocol support
- HTTP CONNECT forward proxy
- L4 TCP stream proxy
- WebSocket proxying
- PROXY protocol v1/v2 support
- DNS and SRV-based dynamic upstream discovery
- Admin REST API (config, health, upstreams, metrics)
- Plugin/module system for custom middleware and handlers
- Graceful shutdown with connection draining
- Structured logging with file rotation
- Prometheus-compatible metrics
- `gatel-passwd` password hashing utility
- `gatel-precompress` static asset compression utility
