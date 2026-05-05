# Security and Code Review Todos

## This pass

- [x] Use real trust roots for HTTPS upstream certificate verification unless `tls-skip-verify` is explicitly enabled.
- [x] Avoid buffering reverse-proxy request bodies when retries are disabled.
- [x] Block CGI script resolution outside the configured CGI root and add focused tests.
- [x] Harden template `include` path resolution so configured roots cannot be escaped, and preserve UTF-8 while scanning templates.
- [x] Bound admin API JSON request bodies to prevent unbounded memory growth.
- [x] Add focused documentation for HTTPS upstream verification and admin API token exposure.

## Follow-up review findings

- [x] Dynamic DNS/SRV upstreams are parsed and documented, but the current `UpstreamPool` selection path is static. This needs a separate design that reconciles dynamic backend snapshots with health checks, connection counters, weighted policies, and retries.
