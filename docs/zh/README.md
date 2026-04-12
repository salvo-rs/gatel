# Gatel 文档

Gatel 是一款用 Rust 构建的高性能反向代理服务器，采用 Caddy 风格的 KDL 配置语法，基于 hyper 1.x、tokio-rustls 和 certon 实现。

---

## 目录

- [项目概览](#项目概览)
- [核心特性](#核心特性)
- [架构概览](#架构概览)
- [文档导航](#文档导航)
- [快速开始](#快速开始)
- [核心依赖](#核心依赖)
- [Feature Flags](#feature-flags)
- [许可证](#许可证)

---

## 项目概览

Gatel 的设计目标是提供一个**配置简洁、性能优异、功能完整**的现代反向代理。它从 Caddy 的设计理念中汲取灵感，同时利用 Rust 生态的高性能异步 I/O 栈来实现底层协议处理。

**关键数据**：

- 约 10,000 行 Rust 代码，38 个源文件
- 配置格式：KDL（KDL Document Language）
- 跨平台支持：Windows、Linux、macOS

---

## 核心特性

### 协议支持

- HTTP/1.1、HTTP/2（自动协商）
- HTTP/3（QUIC，通过 feature flag 启用）
- WebSocket 透明代理
- PROXY Protocol v1（文本）和 v2（二进制）
- FastCGI 协议（兼容 PHP-FPM）

### 反向代理

- 9 种负载均衡策略：round_robin、random、weighted_round_robin、ip_hash、least_conn、uri_hash、header_hash、cookie_hash、first
- 主动健康检查 + 被动健康检查
- 自动重试
- 请求头 / 响应头修改
- DNS 上游动态解析

### TLS 与证书管理

- 通过 certon 自动获取证书（Let's Encrypt、ZeroSSL）
- ACME HTTP-01 挑战
- 手动 PEM 证书配置
- mTLS（双向 TLS）
- 按需 TLS（On-Demand TLS）

### 中间件

- logging — 结构化访问日志
- compress — gzip / zstd / brotli 压缩
- headers — 请求头和响应头操作
- rewrite — URL 重写
- redirect — HTTP 重定向
- basic_auth — 基础认证（bcrypt）
- rate_limit — 令牌桶限流
- ip_filter — CIDR 白名单/黑名单
- cache — LRU 响应缓存
- templates — 模板渲染

### 其他

- 静态文件服务器（MIME 检测、ETag、Range 请求、目录浏览）
- L4 TCP 流代理
- 管理 API（配置查看、热重载、健康状态、上游状态、Prometheus 指标）
- Runtime 控制面（service/route/target 的运行时变更、健康门控激活、drain、runtime TLS）
- 热重载（SIGHUP 信号，ArcSwap 原子切换）
- 优雅关闭（连接排空 + 可配置宽限期）

---

## 架构概览

```
客户端
  |
  v
TCP 接收
  |
  v
[PROXY Protocol 解析] (可选)
  |
  v
TLS 终止 (tokio-rustls + certon)
  |
  v
HTTP 解析 (hyper auto: H1 / H2 / H3)
  |
  v
路由器
  Host 匹配 --> Site
  Path + Matchers --> Route
  |
  v
中间件链
  logging -> ip_filter -> rate_limit -> auth -> rewrite
  -> headers -> compress -> cache -> templates
  |
  v
终端处理器
  ReverseProxy | FastCGI | FileServer | Redirect | Respond
  |
  v
(反向代理路径) 负载均衡器 --> 选择后端 --> hyper-util Client --> 上游服务
```

**请求处理流程**：

1. TCP 连接到达后，可选地解析 PROXY Protocol 头以获取真实客户端地址。
2. 通过 tokio-rustls 进行 TLS 握手，certon 负责证书的自动签发和续期。
3. hyper 解析 HTTP 请求，自动识别 HTTP/1.1 和 HTTP/2 协议。
4. 路由器根据 Host 头匹配到 Site，再根据路径和匹配器找到目标 Route。
5. 请求依次经过中间件链处理。
6. 最终由终端处理器生成响应：反向代理将请求转发至上游、文件服务器返回静态文件、或直接返回固定响应。

---

## 文档导航

| 文档 | 说明 |
|---|---|
| [快速开始](getting-started.md) | 安装、首次运行、基础配置 |
| [配置参考](configuration.md) | 完整的 KDL 配置语法手册 |
| [反向代理](reverse-proxy.md) | 代理、负载均衡、健康检查、重试、请求头 |
| [Runtime 控制面](runtime-control-plane.md) | Runtime service 模型、admin API、drain 语义、runtime TLS、控制器边界 |
| [中间件参考](middleware.md) | 所有内置中间件的详细说明 |
| [TLS 与 ACME](tls-and-acme.md) | TLS 配置、自动证书、mTLS、按需 TLS |
| [高级功能](advanced-features.md) | HTTP/3、WebSocket、FastCGI、流代理等 |

---

## 快速开始

### 安装

```bash
# 从源码编译
git clone https://github.com/salvo-rs/gatel.git
cd gatel
cargo build --release

# 启用 HTTP/3 支持
cargo build --release --features http3

# 启用所有功能
cargo build --release --features "http3,bcrypt"
```

### 最小配置

创建 `gatel.kdl`：

```kdl
site "localhost:8080" {
    route "/" {
        respond "Hello from Gatel!" status=200
    }
}
```

### 启动

```bash
gatel run --config gatel.kdl
```

访问 `http://localhost:8080` 即可看到响应。

更多内容请参阅 [快速开始](getting-started.md)。

---

## 核心依赖

| 库 | 版本 | 用途 |
|---|---|---|
| tokio | 1 | 异步运行时 |
| hyper | 1 | HTTP 协议实现 |
| hyper-util | 0.1 | HTTP 服务端/客户端工具 |
| tokio-rustls | 0.26 | TLS 接受器 |
| rustls | 0.23 | TLS 实现 |
| certon | path | ACME 证书自动管理 |
| kdl | 6 | KDL 配置解析 |
| arc-swap | 1 | 无锁原子配置切换 |
| clap | 4 | CLI 参数解析 |
| tracing | - | 结构化日志 |
| async-compression | - | gzip / zstd / brotli 压缩 |
| dashmap | 6 | 并发哈希表 |
| quinn | 0.11 | QUIC 传输（可选） |
| h3 | 0.0.8 | HTTP/3 协议（可选） |

---

## Feature Flags

| Flag | 说明 |
|---|---|
| `bcrypt` | 启用 bcrypt 密码哈希，用于 basic_auth 中间件 |
| `http3` | 启用 HTTP/3 (QUIC) 支持，依赖 quinn 和 h3 |

编译示例：

```bash
# 仅启用 bcrypt
cargo build --release --features bcrypt

# 仅启用 HTTP/3
cargo build --release --features http3

# 同时启用
cargo build --release --features "bcrypt,http3"
```

---

## 许可证

请参阅项目根目录的 LICENSE 文件。
