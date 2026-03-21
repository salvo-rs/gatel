# Gatel

**一个基于 Rust 构建的高性能、KDL 配置的反向代理和 Web 服务器。**

Gatel 是一个受 Caddy 启发的现代反向代理和 Web 服务器，基于 Hyper 和 Tokio 构建。它使用 [KDL](https://kdl.dev/) 作为配置语言，提供简洁且富有表现力的方式来定义站点、路由和中间件。

## 功能特性

- **KDL 配置** — 人类友好的配置格式，支持直观的嵌套结构
- **反向代理** — 加权负载均衡、健康检查、被动健康监测和自动重试
- **TLS / ACME** — 通过 ACME（Let's Encrypt）自动 HTTPS、手动证书配置和 mTLS
- **HTTP/1.1、HTTP/2、HTTP/3** — 完整协议支持，包括基于 QUIC 的 HTTP/3
- **压缩** — 支持 Gzip、Zstd 和 Brotli 编码
- **静态文件服务** — 高效的文件服务器，支持可配置的根目录
- **速率限制** — 按路由的请求速率限制
- **流代理** — TCP/UDP 流代理
- **管理 API** — 运行时管理接口

## 快速开始

```bash
# 构建
cargo build --release

# 使用配置文件运行
gatel run --config gatel.kdl
```

### 最小配置

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

### 反向代理

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

## 文档

完整中文文档请参阅 [docs/zh](docs/zh/) 目录，英文文档请参阅 [docs/en](docs/en/) 目录。

## 许可证

基于 Apache License 2.0 许可。详情请参阅 [LICENSE](LICENSE)。
