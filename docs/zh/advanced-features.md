# 高级功能

本文档介绍 Gatel 的高级功能，包括 HTTP/3、WebSocket、FastCGI、流代理、PROXY Protocol、匹配器、模板和缓存。

---

## 目录

- [HTTP/3 (QUIC)](#http3-quic)
- [WebSocket 代理](#websocket-代理)
- [FastCGI](#fastcgi)
- [流代理 (TCP)](#流代理-tcp)
- [PROXY Protocol](#proxy-protocol)
- [请求匹配器](#请求匹配器)
- [模板渲染](#模板渲染)
- [响应缓存](#响应缓存)
- [静态文件服务器](#静态文件服务器)
- [管理 API 与可观测性](#管理-api-与可观测性)
- [热重载机制](#热重载机制)
- [优雅关闭](#优雅关闭)

---

## HTTP/3 (QUIC)

HTTP/3 是基于 QUIC 传输协议的 HTTP 版本，提供更低的连接建立延迟和更好的弱网性能。

### 编译启用

HTTP/3 是一个可选功能，需要在编译时启用 `http3` feature flag：

```bash
cargo build --release --features http3
```

依赖库：

| 库 | 版本 | 用途 |
|---|---|---|
| quinn | 0.11 | QUIC 传输协议实现 |
| h3 | 0.0.8 | HTTP/3 协议实现 |

### 配置

```kdl
global {
    http3 true
    https ":443"
}

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

### 工作原理

1. Gatel 在 HTTPS 端口同时监听 TCP（用于 HTTP/1.1 和 HTTP/2）和 UDP（用于 QUIC/HTTP/3）。
2. 在 HTTP/1.1 和 HTTP/2 的响应中自动添加 `Alt-Svc` 头，告知客户端可以使用 HTTP/3。
3. 支持 HTTP/3 的客户端（如现代浏览器）会自动升级到 HTTP/3。

### Alt-Svc 头

Gatel 自动在响应中添加：

```
Alt-Svc: h3=":443"; ma=86400
```

这告诉浏览器该服务支持 HTTP/3，有效期 86400 秒（24 小时）。

### 注意事项

- HTTP/3 需要 TLS（QUIC 内置了 TLS 1.3）。
- 防火墙需要开放 UDP 端口（通常与 HTTPS 相同的端口号）。
- 目前 HTTP/3 仅用于客户端到 Gatel 的连接，Gatel 到上游仍使用 HTTP/1.1 或 HTTP/2。

### 验证 HTTP/3

```bash
# 使用 curl（需要 HTTP/3 支持编译的 curl）
curl --http3 https://example.com/

# 使用浏览器开发者工具
# 在 Network 面板中，Protocol 列会显示 "h3"
```

---

## WebSocket 代理

Gatel 自动识别和透明代理 WebSocket 连接。

### 配置

WebSocket 代理不需要特殊配置，标准的 `proxy` 指令即可：

```kdl
site "example.com" {
    route "/ws" {
        proxy "127.0.0.1:8001"
    }
}
```

### 工作原理

1. 客户端发送带有 `Upgrade: websocket` 头的 HTTP 请求。
2. Gatel 检测到 WebSocket 升级请求。
3. 将升级请求转发到上游。
4. 上游返回 `101 Switching Protocols` 响应。
5. Gatel 将响应返回给客户端。
6. 连接升级完成后，Gatel 在客户端和上游之间双向转发 WebSocket 帧。

### 示例：聊天应用

```kdl
site "chat.example.com" {
    // WebSocket 连接
    route "/ws" {
        proxy "127.0.0.1:8001"
    }

    // 静态页面
    route "/" {
        root "/var/www/chat"
        file-server
    }
}
```

### 与中间件的兼容性

WebSocket 连接在升级后会绕过大部分中间件（如 compress、cache），因为 WebSocket 帧不是标准的 HTTP 请求/响应。以下中间件在升级前仍然生效：

- logging — 记录初始握手请求
- ip-filter — 在连接建立前检查 IP
- rate-limit — 限制握手请求频率
- basic-auth — 验证握手请求的认证信息

---

## FastCGI

Gatel 实现了完整的 FastCGI 协议，可以与 PHP-FPM 等 FastCGI 服务通信。

### 基础配置

```kdl
site "php.example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/html"
            split ".php"
            index "index.php"
        }
    }

    // 非 PHP 文件作为静态资源
    route "/" {
        root "/var/www/html"
        file-server
    }
}
```

### 参数

**连接地址**（第一个参数）：

```kdl
// TCP 连接
fastcgi "127.0.0.1:9000" { }

// 也可以使用主机名
fastcgi "php-fpm:9000" { }
```

### 子节点

| 节点 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `root` | string | 是 | 文档根目录，对应 PHP 的 `DOCUMENT_ROOT` |
| `split` | string | 否 | 路径分割标记，通常为 `".php"` |
| `index` | string | 否 | 默认索引文件，通常为 `"index.php"` |
| `env` | key-value | 否 | 自定义环境变量（可多次使用） |

### 环境变量

Gatel 自动设置以下 FastCGI 环境变量：

| 变量 | 说明 |
|---|---|
| `SCRIPT_FILENAME` | 完整的脚本文件路径 |
| `SCRIPT_NAME` | 脚本名称（相对路径） |
| `DOCUMENT_ROOT` | 文档根目录 |
| `QUERY_STRING` | URL 查询字符串 |
| `REQUEST_METHOD` | HTTP 方法 |
| `REQUEST_URI` | 完整的请求 URI |
| `SERVER_NAME` | 服务器名称 |
| `SERVER_PORT` | 服务器端口 |
| `REMOTE_ADDR` | 客户端 IP |
| `CONTENT_TYPE` | 请求体类型 |
| `CONTENT_LENGTH` | 请求体长度 |

自定义环境变量：

```kdl
fastcgi "127.0.0.1:9000" {
    root "/var/www/html"
    split ".php"
    index "index.php"
    env "DB_HOST" "localhost"
    env "DB_PORT" "3306"
    env "DB_NAME" "myapp"
    env "APP_ENV" "production"
}
```

### split 参数详解

`split` 参数用于从 URI 中提取脚本路径和 PATH_INFO：

```
请求: /blog/index.php/article/123
split: ".php"

结果:
  SCRIPT_NAME  = /blog/index.php
  PATH_INFO    = /article/123
```

### 与 PHP-FPM 配合

PHP-FPM 配置示例（`www.conf`）：

```ini
[www]
listen = 127.0.0.1:9000
pm = dynamic
pm.max_children = 50
pm.start_servers = 5
pm.min_spare_servers = 5
pm.max_spare_servers = 35
```

Gatel 配置：

```kdl
site "php-app.example.com" {
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/php-app"
            split ".php"
            index "index.php"
        }
    }

    route "/" {
        root "/var/www/php-app"
        file-server
    }
}
```

### WordPress 示例

```kdl
site "wordpress.example.com" {
    // PHP 文件
    route "*.php" {
        fastcgi "127.0.0.1:9000" {
            root "/var/www/wordpress"
            split ".php"
            index "index.php"
        }
    }

    // 静态文件
    route "/wp-content/*" {
        encode "gzip"
        cache max-entries=5000 max-age="3600s"
        root "/var/www/wordpress"
        file-server
    }

    // 伪静态（WordPress URL 重写）
    route "/" {
        rewrite "/" "/index.php"
        fastcgi "127.0.0.1:9000" {
            root "/var/www/wordpress"
            split ".php"
            index "index.php"
        }
    }
}
```

---

## 流代理 (TCP)

Gatel 支持 L4 层的 TCP 流代理，实现双向数据转发，无需解析应用层协议。

### 配置

```kdl
stream {
    listen ":3306" {
        proxy "mysql-primary:3306"
    }
}
```

### 工作原理

1. Gatel 在指定端口监听 TCP 连接。
2. 客户端连接到达后，Gatel 建立到上游的 TCP 连接。
3. 在客户端和上游之间双向转发数据，直到任一方关闭连接。

### 多服务代理

```kdl
stream {
    // MySQL 代理
    listen ":3306" {
        proxy "mysql-server:3306"
    }

    // Redis 代理
    listen ":6379" {
        proxy "redis-server:6379"
    }

    // PostgreSQL 代理
    listen ":5432" {
        proxy "postgres-server:5432"
    }

    // 自定义 TCP 服务
    listen ":9999" {
        proxy "backend:9999"
    }
}
```

### 适用场景

| 场景 | 说明 |
|---|---|
| 数据库代理 | MySQL、PostgreSQL、MongoDB 等 |
| 缓存代理 | Redis、Memcached 等 |
| 消息队列 | RabbitMQ、Kafka 等的 TCP 连接 |
| 自定义协议 | 任何基于 TCP 的协议 |

### 与 HTTP 代理的区别

| 特性 | HTTP 代理 | TCP 流代理 |
|---|---|---|
| 协议感知 | 是 | 否 |
| 负载均衡 | 9 种策略 | 无 |
| 健康检查 | 支持 | 不支持 |
| 中间件 | 支持 | 不支持 |
| TLS 终止 | 支持 | 不支持 |
| 配置位置 | `site` > `route` | `stream` > `listen` |

---

## PROXY Protocol

PROXY Protocol 允许在代理链中传递真实的客户端连接信息（源 IP、源端口、目的 IP、目的端口）。

### 启用

```kdl
global {
    proxy-protocol true
}
```

### 支持的版本

| 版本 | 格式 | 说明 |
|---|---|---|
| v1 | 文本 | `PROXY TCP4 192.168.1.1 10.0.0.1 56789 80\r\n` |
| v2 | 二进制 | 二进制编码头，支持更多信息 |

Gatel 自动检测并解析两种版本。

### 工作原理

1. 前端代理（如 HAProxy、AWS ELB）在建立连接后，先发送 PROXY Protocol 头。
2. Gatel 解析头部，提取真实客户端信息。
3. 后续的请求处理中，`remote_ip` 等变量使用解析出的真实地址。

### 典型部署架构

```
客户端 (1.2.3.4) → HAProxy (PROXY Protocol) → Gatel → 后端
```

HAProxy 配置示例：

```
backend gatel_backend
    server s1 10.0.0.1:443 send-proxy-v2
```

### 与 X-Forwarded-For 的区别

| 特性 | PROXY Protocol | X-Forwarded-For |
|---|---|---|
| 层级 | L4（传输层） | L7（应用层） |
| 可伪造 | 否 | 是（客户端可自行设置） |
| 性能 | 更好（仅连接建立时解析） | 每请求解析 |
| 适用 | 所有 TCP 协议 | 仅 HTTP |

### 注意事项

- 仅在前端确实发送 PROXY Protocol 时才启用此选项。
- 如果启用了 PROXY Protocol 但连接不包含协议头，连接会被拒绝。
- 不要在直接面向客户端的场景下启用（客户端不会发送 PROXY Protocol 头）。

---

## 请求匹配器

匹配器提供灵活的请求匹配能力，在路径匹配的基础上增加额外条件。

### 匹配器类型总览

| 匹配器 | 说明 | 示例 |
|---|---|---|
| `method` | HTTP 方法 | `match method="GET,POST"` |
| `header` | 请求头 | `match header="Accept" pattern="*json*"` |
| `query` | 查询参数 | `match query="format" value="json"` |
| `remote-ip` | 客户端 IP | `match remote-ip="10.0.0.0/8"` |
| `protocol` | 协议 | `match protocol="https"` |
| `expression` | 组合表达式 | `match expression="{method} == GET"` |
| `not` | 否定 | `match not { ... }` |

### method — HTTP 方法匹配

匹配指定的 HTTP 方法，多个方法用逗号分隔。

```kdl
route "/api/*" {
    // 仅允许 GET 和 POST
    match method="GET,POST"
    proxy "127.0.0.1:3000"
}
```

支持的方法：`GET`、`POST`、`PUT`、`DELETE`、`PATCH`、`HEAD`、`OPTIONS`。

### header — 请求头匹配

匹配指定请求头的值，支持通配符。

```kdl
route "/api/*" {
    // 匹配 JSON 请求
    match header="Content-Type" pattern="application/json*"
    proxy "127.0.0.1:3000"
}

route "/api/*" {
    // 匹配带有特定头的请求
    match header="X-API-Key" pattern="*"
    proxy "127.0.0.1:3000"
}
```

### query — 查询参数匹配

匹配 URL 查询参数。

```kdl
route "/search" {
    match query="format" value="json"
    // /search?format=json → 匹配
    // /search?format=xml  → 不匹配
    proxy "127.0.0.1:3000"
}
```

### remote-ip — 客户端 IP 匹配

基于客户端 IP 地址匹配，使用 CIDR 表示法。

```kdl
route "/internal/*" {
    match remote-ip="10.0.0.0/8"
    proxy "127.0.0.1:3000"
}
```

### protocol — 协议匹配

匹配请求协议。

```kdl
// 仅 HTTPS 请求
route "/secure/*" {
    match protocol="https"
    proxy "127.0.0.1:3000"
}

// HTTP 请求重定向到 HTTPS
route "/" {
    match protocol="http"
    redirect "https://{host}{path}" permanent=true
}
```

### expression — 表达式匹配

使用表达式语法组合多种条件。

```kdl
route "/api/*" {
    match expression="{method} == GET && {path} ~ /api/public/*"
    proxy "127.0.0.1:3000"
}
```

**可用变量**：

| 变量 | 说明 |
|---|---|
| `{method}` | HTTP 方法 |
| `{path}` | 请求路径 |
| `{host}` | 主机名 |
| `{remote_ip}` | 客户端 IP |
| `{protocol}` | 协议（http / https） |

**运算符**：

| 运算符 | 说明 | 示例 |
|---|---|---|
| `==` | 等于 | `{method} == GET` |
| `!=` | 不等于 | `{method} != DELETE` |
| `~` | 通配符匹配 | `{path} ~ /api/*` |
| `&&` | 逻辑与 | `{method} == GET && {protocol} == https` |
| `\|\|` | 逻辑或 | `{method} == GET \|\| {method} == HEAD` |

**复杂表达式示例**：

```kdl
// API 公开端点：仅 HTTPS + GET
route "/api/public/*" {
    match expression="{protocol} == https && {method} == GET"
    proxy "127.0.0.1:3000"
}

// 允许多种方法
route "/api/data" {
    match expression="{method} == GET || {method} == POST || {method} == PUT"
    proxy "127.0.0.1:3000"
}
```

### not — 否定匹配

反转内部匹配器的结果。

```kdl
route "/api/*" {
    // 不是来自内网的请求
    match not {
        match remote-ip="10.0.0.0/8"
    }
    // 对外部请求进行限流
    rate-limit window="1m" max=50
    proxy "127.0.0.1:3000"
}
```

### 匹配器组合

同一 route 中的多个 `match` 指令之间是 AND 关系：

```kdl
route "/api/admin/*" {
    // 同时满足以下所有条件
    match method="GET,POST"          // AND
    match protocol="https"           // AND
    match remote-ip="10.0.0.0/8"    // AND
    match header="Authorization" pattern="Bearer *"

    proxy "127.0.0.1:3001"
}
```

---

## 模板渲染

Gatel 支持服务端模板渲染，可以在静态文件中嵌入动态内容。

### 配置

```kdl
site "example.com" {
    route "/" {
        templates root="/var/www/templates"
        root "/var/www/html"
        file-server
    }
}
```

### 属性

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `root` | string | 是 | 模板文件根目录 |

### 工作流程

1. 请求到达时，先检查请求的文件是否是模板文件。
2. 如果是模板文件，读取并渲染模板。
3. 将渲染结果作为响应返回。

---

## 响应缓存

Gatel 内置基于 LRU 算法的响应缓存，可以显著减少后端负载。

### 配置

```kdl
route "/api/*" {
    cache max-entries=1000 max-age="300s"
    proxy "127.0.0.1:3000"
}
```

### 属性

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `max-entries` | int | 是 | 最大缓存条目数 |
| `max-age` | duration | 是 | 缓存最大存活时间 |

### 缓存行为

**命中条件**：

- 请求方法为 GET 或 HEAD。
- 缓存键匹配（基于 URL 和相关请求头）。
- 缓存条目未过期。

**不缓存的情况**：

- 非 GET/HEAD 请求。
- 响应状态码非 2xx。
- 响应包含 `Cache-Control: no-store` 头。
- 请求包含 `Cache-Control: no-cache` 头。

### 缓存策略示例

```kdl
site "example.com" {
    // 公开 API — 短期缓存
    route "/api/public/*" {
        cache max-entries=5000 max-age="60s"
        proxy "127.0.0.1:3000"
    }

    // 静态资源 — 长期缓存
    route "/assets/*" {
        cache max-entries=10000 max-age="86400s"
        root "/var/www/static"
        file-server
    }

    // 私有 API — 不缓存
    route "/api/private/*" {
        proxy "127.0.0.1:3000"
    }
}
```

### 实现细节

- 缓存使用 dashmap（并发哈希表）存储，支持高并发读写。
- LRU 淘汰策略确保在内存有限时优先保留热点数据。
- 缓存是进程内的，重启后清空。

---

## 静态文件服务器

Gatel 内置高性能的静态文件服务器。

### 基础配置

```kdl
route "/" {
    root "/var/www/html"
    file-server
}
```

### 启用目录浏览

```kdl
route "/" {
    root "/var/www/html"
    file-server browse=true
}
```

### 功能特性

| 功能 | 说明 |
|---|---|
| MIME 检测 | 根据文件扩展名自动设置 `Content-Type` |
| ETag | 基于文件内容生成 ETag，支持条件请求 |
| Last-Modified | 基于文件修改时间，支持条件请求 |
| Range 请求 | 支持断点续传和部分内容请求 |
| 目录浏览 | 可选功能，显示目录内容列表 |

### 条件请求

文件服务器自动处理条件请求：

- `If-None-Match` — 与 ETag 比较，匹配则返回 304。
- `If-Modified-Since` — 与修改时间比较，未变化则返回 304。

### 配合压缩和缓存

```kdl
route "/assets/*" {
    encode "gzip" "brotli"
    cache max-entries=10000 max-age="3600s"
    root "/var/www/assets"
    file-server
}
```

---

## 管理 API 与可观测性

### 管理 API

```kdl
global {
    admin ":2019"
}
```

| 端点 | 方法 | 说明 |
|---|---|---|
| `/config` | GET | 返回当前运行的完整配置 |
| `/config/reload` | POST | 触发配置热重载 |
| `/health` | GET | 服务健康状态 |
| `/upstreams` | GET | 所有上游后端的健康状态和连接数 |
| `/metrics` | GET | Prometheus 格式的指标数据 |

### Prometheus 指标

`/metrics` 端点输出 Prometheus 格式的指标：

```bash
curl http://localhost:2019/metrics
```

输出示例：

```
# HELP gatel_requests_total Total number of requests
# TYPE gatel_requests_total counter
gatel_requests_total{site="api.example.com",status="200"} 12345
gatel_requests_total{site="api.example.com",status="404"} 23

# HELP gatel_request_duration_seconds Request duration in seconds
# TYPE gatel_request_duration_seconds histogram
gatel_request_duration_seconds_bucket{site="api.example.com",le="0.01"} 10000
gatel_request_duration_seconds_bucket{site="api.example.com",le="0.1"} 12000
gatel_request_duration_seconds_bucket{site="api.example.com",le="1"} 12300

# HELP gatel_upstream_health Upstream health status
# TYPE gatel_upstream_health gauge
gatel_upstream_health{upstream="10.0.1.1:3000"} 1
gatel_upstream_health{upstream="10.0.1.2:3000"} 0
```

### 与 Prometheus/Grafana 集成

Prometheus 配置：

```yaml
scrape_configs:
  - job_name: 'gatel'
    scrape_interval: 15s
    static_configs:
      - targets: ['localhost:2019']
    metrics_path: '/metrics'
```

### 结构化日志

Gatel 使用 tracing 框架生成结构化日志：

```kdl
global {
    log level="info" format="json"
}
```

JSON 格式的日志便于与日志收集系统（ELK、Loki 等）集成：

```json
{"timestamp":"2024-01-15T10:30:00.123Z","level":"INFO","target":"gatel::proxy","message":"request completed","method":"GET","path":"/api/users","status":200,"duration_ms":12,"upstream":"10.0.1.1:3000"}
```

---

## 热重载机制

Gatel 支持无中断的配置热重载。

### 触发方式

```bash
# 方式一：发送 SIGHUP 信号
kill -SIGHUP $(pidof gatel)

# 方式二：使用 CLI
gatel reload

# 方式三：管理 API
curl -X POST http://localhost:2019/config/reload
```

### 实现原理

Gatel 使用 ArcSwap 实现无锁原子配置切换：

1. 接收到重载信号。
2. 读取并解析新的配置文件。
3. 验证新配置的正确性。
4. 如果验证通过，通过 `ArcSwap::store()` 原子替换配置。
5. 新请求使用新配置处理。
6. 旧配置的引用计数归零后自动释放。

### 重载范围

热重载会更新以下内容：

- 站点配置（route、中间件、处理器）
- 上游列表和负载均衡策略
- 健康检查配置
- 手动 TLS 证书（重新加载证书文件）

以下内容不受热重载影响（需要重启）：

- 监听地址变更（`http`、`https` 端口）
- HTTP/3 开关
- 管理 API 地址

### 错误处理

如果新配置无效：

- 保持当前配置不变。
- 在日志中记录详细错误信息。
- 管理 API 返回错误响应。

---

## 优雅关闭

当 Gatel 接收到关闭信号时，执行优雅关闭流程。

### 触发方式

- `SIGTERM` — 标准关闭信号（如 `kill` 命令、`systemctl stop`）
- `SIGINT` — 中断信号（如 Ctrl+C）

### 关闭流程

1. **停止接受新连接** — 立即停止 TCP 监听。
2. **等待活跃连接** — 等待正在处理的请求完成。
3. **宽限期超时** — 如果超过宽限期仍有未完成的连接，强制关闭。
4. **清理资源** — 释放所有资源，进程退出。

### 配置宽限期

```kdl
global {
    grace-period "30s"
}
```

宽限期的选择建议：

| 场景 | 建议值 | 说明 |
|---|---|---|
| 短连接为主的 API | `10s` - `30s` | 大部分请求会快速完成 |
| 长连接（WebSocket） | `60s` - `120s` | 给长连接更多时间关闭 |
| 流式下载服务 | `120s` - `300s` | 大文件下载可能需要更长时间 |

### systemd 集成

```ini
[Unit]
Description=Gatel Reverse Proxy
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/gatel run --config /etc/gatel/gatel.kdl
ExecReload=/bin/kill -SIGHUP $MAINPID
Restart=on-failure
RestartSec=5
TimeoutStopSec=60

[Install]
WantedBy=multi-user.target
```

注意 `TimeoutStopSec` 应大于 Gatel 的 `grace-period`，以确保 systemd 不会在宽限期结束前强制终止进程。
