# Gatel

**A high-performance, KDL-configured reverse proxy and web server built with Rust.**

Gatel is a modern reverse proxy and web server inspired by Caddy, powered by Hyper and Tokio. It uses [KDL](https://kdl.dev/) as its configuration language, providing a clean and expressive way to define sites, routes, and middleware.

## Features

- **KDL Configuration** — Human-friendly config format with intuitive nesting
- **Reverse Proxy** — Weighted load balancing, health checks, passive health monitoring, and automatic retries
- **TLS / ACME** — Automatic HTTPS via ACME (Let's Encrypt), manual certificate support, and mTLS
- **HTTP/1.1, HTTP/2, HTTP/3** — Full protocol support including QUIC-based HTTP/3
- **Compression** — Gzip, Zstd, and Brotli encoding
- **Static File Serving** — Efficient file server with configurable root directories
- **Rate Limiting** — Per-route request rate limiting
- **Stream Proxy** — TCP/UDP stream proxying
- **Admin API** — Runtime management endpoint

## Quick Start

```bash
# Build
cargo build --release

# Run with a config file
gatel run --config gatel.kdl
```

### Minimal Configuration

```kdl
global {
    http ":8080"
}

site "localhost" {
    route "/*" {
        respond "Hello from Gatel!" status=200
    }
}
```

### Reverse Proxy

```kdl
global {
    http ":80"
}

site "example.com" {
    route "/api/*" {
        proxy {
            upstream "127.0.0.1:3001" weight=3
            upstream "127.0.0.1:3002" weight=1
            lb "weighted_round_robin"
            health-check uri="/health" interval="10s"
        }
    }
    route "/*" {
        root "/var/www/html"
        file-server
    }
}
```

## Documentation

See the [docs/en](docs/en/) directory for full English documentation, or [docs/zh](docs/zh/) for Chinese documentation.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

[中文文档](README.zh.md)
