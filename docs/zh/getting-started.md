# 快速开始

本文档介绍如何安装 Gatel、编写第一份配置文件并启动服务。

---

## 目录

- [系统要求](#系统要求)
- [安装](#安装)
- [CLI 命令](#cli-命令)
- [第一份配置](#第一份配置)
- [基础反向代理](#基础反向代理)
- [静态文件服务](#静态文件服务)
- [启用 HTTPS](#启用-https)
- [管理 API](#管理-api)
- [热重载](#热重载)
- [优雅关闭](#优雅关闭)
- [日志配置](#日志配置)
- [下一步](#下一步)

---

## 系统要求

- Rust 工具链（编译时需要）
- 操作系统：Windows、Linux、macOS
- 如果需要 HTTP/3，编译时需要启用 `http3` feature flag

---

## 安装

### 从源码编译

```bash
git clone https://github.com/salvo-rs/gatel.git
cd gatel
cargo build --release
```

编译完成后，二进制文件位于 `target/release/gatel`。

### 启用可选功能

```bash
# 启用 HTTP/3 (QUIC) 支持
cargo build --release --features http3

# 启用 bcrypt 密码哈希（用于基础认证）
cargo build --release --features bcrypt

# 启用所有可选功能
cargo build --release --features "http3,bcrypt"
```

### 验证安装

```bash
gatel --help
```

---

## CLI 命令

Gatel 提供三个核心命令：

### run — 启动服务器

```bash
gatel run --config gatel.kdl
```

以指定的配置文件启动服务器。如果不提供 `--config` 参数，默认查找当前目录的 `gatel.kdl`。

### validate — 验证配置

```bash
gatel validate --config gatel.kdl
```

仅解析和验证配置文件，不启动服务器。适合在部署前检查配置是否正确。

### reload — 热重载

```bash
gatel reload
```

向正在运行的 Gatel 进程发送 SIGHUP 信号，触发配置热重载。Gatel 通过 ArcSwap 实现原子配置切换，重载期间不会中断现有连接。

---

## 第一份配置

Gatel 使用 KDL（KDL Document Language）作为配置格式。创建文件 `gatel.kdl`：

```kdl
// 最简配置：在 8080 端口返回固定响应
site "localhost:8080" {
    route "/" {
        respond "Hello from Gatel!" status=200
    }
}
```

启动服务器：

```bash
gatel run --config gatel.kdl
```

在浏览器中访问 `http://localhost:8080`，你会看到 "Hello from Gatel!"。

---

## 基础反向代理

将请求转发到后端服务：

```kdl
site "localhost:8080" {
    route "/api/*" {
        proxy "127.0.0.1:3000"
    }

    route "/" {
        respond "Welcome" status=200
    }
}
```

这个配置将 `/api/*` 路径的所有请求转发到 `127.0.0.1:3000`，其他请求返回 "Welcome"。

### 多后端负载均衡

```kdl
site "localhost:8080" {
    route "/" {
        proxy {
            upstream "127.0.0.1:3001"
            upstream "127.0.0.1:3002"
            upstream "127.0.0.1:3003"
            lb "round_robin"
        }
    }
}
```

---

## 静态文件服务

提供静态文件服务：

```kdl
site "localhost:8080" {
    route "/" {
        root "/var/www/html"
        file-server
    }
}
```

启用目录浏览：

```kdl
site "localhost:8080" {
    route "/" {
        root "/var/www/html"
        file-server browse=true
    }
}
```

文件服务器自动支持：

- MIME 类型检测
- ETag 生成
- Last-Modified 头
- Range 请求（断点续传）
- 目录浏览（需显式启用）

---

## 启用 HTTPS

### 自动 HTTPS（ACME）

Gatel 可以通过 ACME 协议自动从 Let's Encrypt 或 ZeroSSL 获取 TLS 证书：

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

site "example.com" {
    route "/" {
        proxy "127.0.0.1:3000"
    }
}
```

当 site 的主机名是一个真实域名（非 localhost）时，Gatel 会自动为该域名签发证书。

### 手动证书

```kdl
site "example.com" {
    tls {
        cert "/etc/gatel/certs/example.com.pem"
        key "/etc/gatel/certs/example.com-key.pem"
    }

    route "/" {
        proxy "127.0.0.1:3000"
    }
}
```

更多 TLS 配置请参阅 [TLS 与 ACME](tls-and-acme.md)。

---

## 管理 API

Gatel 内置管理 API，默认监听在 `:2019`：

```kdl
global {
    admin ":2019"
}
```

### 可用端点

| 端点 | 方法 | 说明 |
|---|---|---|
| `/config` | GET | 获取当前运行配置 |
| `/config/reload` | POST | 触发配置热重载 |
| `/health` | GET | 健康检查端点 |
| `/upstreams` | GET | 查看所有上游后端状态 |
| `/metrics` | GET | Prometheus 格式指标 |

### 使用示例

```bash
# 查看当前配置
curl http://localhost:2019/config

# 触发热重载
curl -X POST http://localhost:2019/config/reload

# 检查服务健康状态
curl http://localhost:2019/health

# 查看上游后端状态
curl http://localhost:2019/upstreams

# 获取 Prometheus 指标
curl http://localhost:2019/metrics
```

---

## 热重载

Gatel 支持两种热重载方式：

### 方式一：SIGHUP 信号

```bash
# 向 Gatel 进程发送 SIGHUP
kill -SIGHUP $(pidof gatel)

# 或使用 CLI
gatel reload
```

### 方式二：管理 API

```bash
curl -X POST http://localhost:2019/config/reload
```

热重载的工作原理：

1. Gatel 接收到重载信号后，重新读取并解析配置文件。
2. 如果新配置有效，通过 ArcSwap 原子切换到新配置。
3. 现有连接继续使用旧配置处理完成，新连接使用新配置。
4. 如果新配置无效，保持旧配置不变，并记录错误日志。

---

## 优雅关闭

当 Gatel 接收到关闭信号（SIGTERM 或 SIGINT）时，会执行优雅关闭：

1. 停止接受新连接。
2. 等待现有连接处理完成。
3. 超过宽限期后强制关闭剩余连接。

宽限期默认为 30 秒，可通过 `grace-period` 配置：

```kdl
global {
    grace-period "30s"
}
```

---

## 日志配置

Gatel 使用结构化日志（基于 tracing），支持两种输出格式：

### 人类可读格式（默认）

```kdl
global {
    log level="info" format="pretty"
}
```

### JSON 格式

```kdl
global {
    log level="info" format="json"
}
```

可用日志级别：`trace`、`debug`、`info`、`warn`、`error`。

生产环境建议使用 `info` 级别配合 `json` 格式，便于日志收集系统解析。

---

## 下一步

- [配置参考](configuration.md) — 了解完整的 KDL 配置语法
- [反向代理](reverse-proxy.md) — 深入了解代理、负载均衡和健康检查
- [中间件参考](middleware.md) — 了解所有可用中间件
- [TLS 与 ACME](tls-and-acme.md) — 配置 HTTPS 和证书管理
- [高级功能](advanced-features.md) — HTTP/3、FastCGI、流代理等高级用法
