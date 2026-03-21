# 中间件参考

本文档详细介绍 Gatel 的所有内置中间件。中间件在路由的终端处理器之前执行，按声明顺序依次处理请求。

---

## 目录

- [中间件概述](#中间件概述)
- [中间件执行顺序](#中间件执行顺序)
- [logging — 访问日志](#logging--访问日志)
- [encode — 响应压缩](#encode--响应压缩)
- [headers — 请求头操作](#headers--请求头操作)
- [rewrite — URL 重写](#rewrite--url-重写)
- [redirect — HTTP 重定向](#redirect--http-重定向)
- [basic-auth — 基础认证](#basic-auth--基础认证)
- [rate-limit — 限流](#rate-limit--限流)
- [ip-filter — IP 过滤](#ip-filter--ip-过滤)
- [cache — 响应缓存](#cache--响应缓存)
- [templates — 模板渲染](#templates--模板渲染)
- [中间件组合示例](#中间件组合示例)

---

## 中间件概述

中间件是 Gatel 请求处理管道中的核心组件。每个中间件可以：

- **检查请求**：读取请求头、路径、方法等信息。
- **修改请求**：改变请求头、重写路径等。
- **短路响应**：直接返回响应而不继续向下传递（如认证失败、限流触发）。
- **修改响应**：压缩响应体、添加响应头等。

中间件配置在 `route` 块内，处于匹配器和处理器之间：

```kdl
route "/api/*" {
    // 匹配器
    match method="GET,POST"

    // 中间件（按声明顺序执行）
    logging
    ip-filter { allow "10.0.0.0/8"; deny "0.0.0.0/0" }
    rate-limit window="1m" max=100
    basic-auth { user "admin" hash="$2b$12$..." }
    encode "gzip"
    cache max-entries=1000 max-age="300s"

    // 处理器
    proxy "127.0.0.1:3000"
}
```

---

## 中间件执行顺序

建议按以下顺序组织中间件，以获得最佳效果：

```
1. logging        — 记录所有请求（包括被后续中间件拒绝的）
2. ip-filter      — 尽早拒绝不允许的 IP
3. rate-limit     — 在认证之前限流，防止暴力破解
4. basic-auth     — 身份认证
5. rewrite        — URL 重写
6. headers        — 请求头修改
7. encode         — 响应压缩
8. cache          — 响应缓存
9. templates      — 模板渲染
```

这不是强制要求，但遵循此顺序可以避免潜在问题。例如，将 `ip-filter` 放在 `logging` 之后，可以记录被拒绝的请求；将 `rate-limit` 放在 `basic-auth` 之前，可以防止密码暴力破解。

---

## logging -- 访问日志

记录每个请求的结构化访问日志。

### 配置

```kdl
route "/" {
    logging
    proxy "127.0.0.1:3000"
}
```

### 输出格式

日志通过 tracing 框架输出，格式由全局 `log` 配置决定：

**pretty 格式**：

```
2024-01-15T10:30:00Z INFO request method=GET path=/api/users status=200 duration=12ms remote_ip=192.168.1.100
```

**json 格式**：

```json
{"timestamp":"2024-01-15T10:30:00Z","level":"INFO","message":"request","method":"GET","path":"/api/users","status":200,"duration_ms":12,"remote_ip":"192.168.1.100"}
```

### 记录的字段

| 字段 | 说明 |
|---|---|
| `method` | HTTP 方法 |
| `path` | 请求路径 |
| `status` | 响应状态码 |
| `duration` | 请求处理耗时 |
| `remote_ip` | 客户端 IP |
| `user_agent` | User-Agent 头 |
| `bytes` | 响应体大小 |

---

## encode -- 响应压缩

对响应体进行压缩，减少传输数据量。

### 配置

```kdl
// 启用单种压缩算法
route "/" {
    encode "gzip"
    proxy "127.0.0.1:3000"
}

// 启用多种压缩算法
route "/" {
    encode "gzip" "zstd" "brotli"
    proxy "127.0.0.1:3000"
}
```

### 支持的算法

| 算法 | 配置值 | 说明 |
|---|---|---|
| gzip | `"gzip"` | 兼容性最好，所有浏览器支持 |
| Zstandard | `"zstd"` | 更好的压缩率和速度，现代浏览器支持 |
| Brotli | `"brotli"` | 最佳压缩率，现代浏览器支持 |

### 工作原理

1. Gatel 检查请求的 `Accept-Encoding` 头。
2. 根据客户端支持的算法和配置的算法列表，选择最优算法。
3. 对响应体进行压缩，设置 `Content-Encoding` 头。
4. 如果客户端不支持任何已配置的算法，不压缩直接传输。

### 优先级

当配置多种算法时，Gatel 按以下优先级选择（前提是客户端支持）：

1. brotli（最佳压缩率）
2. zstd（优秀的压缩率和速度平衡）
3. gzip（广泛兼容）

### 注意事项

- 压缩仅对文本类型的响应有效（如 HTML、CSS、JS、JSON、XML）。
- 已经压缩的内容（如图片、视频）不会重复压缩。
- 压缩会消耗 CPU 资源，在高并发场景下注意监控 CPU 使用率。

---

## headers -- 请求头操作

在 route 级别操作请求头和响应头。

> 注意：代理场景下的请求头操作通常使用 `proxy` 内的 `header-up` 和 `header-down`，参见 [反向代理](reverse-proxy.md)。`headers` 中间件适用于所有处理器类型。

### 配置

```kdl
route "/" {
    headers {
        // 设置响应头
        response "X-Frame-Options" "DENY"
        response "X-Content-Type-Options" "nosniff"
        response "Strict-Transport-Security" "max-age=31536000; includeSubDomains"
        response "Referrer-Policy" "strict-origin-when-cross-origin"

        // 设置请求头
        request "X-Request-Source" "gatel"

        // 删除头（前缀 - 号）
        response "-Server"
    }
    proxy "127.0.0.1:3000"
}
```

### 安全头配置示例

```kdl
route "/" {
    headers {
        response "Strict-Transport-Security" "max-age=31536000; includeSubDomains; preload"
        response "X-Frame-Options" "DENY"
        response "X-Content-Type-Options" "nosniff"
        response "X-XSS-Protection" "1; mode=block"
        response "Referrer-Policy" "strict-origin-when-cross-origin"
        response "Content-Security-Policy" "default-src 'self'"
        response "Permissions-Policy" "camera=(), microphone=(), geolocation=()"
    }
    file-server
}
```

---

## rewrite -- URL 重写

在处理器接收请求之前，重写请求的 URI 路径。

### 配置

```kdl
route "/old-api/*" {
    rewrite "/old-api" "/new-api"
    proxy "127.0.0.1:3000"
}
```

上面的配置将 `/old-api/users` 重写为 `/new-api/users` 后转发给后端。

### 示例

```kdl
// 移除路径前缀
route "/v1/*" {
    rewrite "/v1" ""
    proxy "127.0.0.1:3000"
}
// /v1/users -> /users

// 添加路径前缀
route "/api/*" {
    rewrite "/api" "/internal/api"
    proxy "127.0.0.1:3000"
}
// /api/users -> /internal/api/users
```

### 与 redirect 的区别

- `rewrite`：内部重写，客户端不感知，URL 栏不变。
- `redirect`：返回 301/302 响应，客户端重新发起请求，URL 栏改变。

---

## redirect -- HTTP 重定向

返回 HTTP 重定向响应。

### 配置

```kdl
// 临时重定向（302）
route "/old-page" {
    redirect "/new-page"
}

// 永久重定向（301）
route "/old-page" {
    redirect "/new-page" permanent=true
}

// 使用变量
route "/" {
    redirect "https://{host}{path}" permanent=true
}
```

### 属性

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `permanent` | bool | `false` | `true` 返回 301，`false` 返回 302 |

### 常见用法

```kdl
// HTTP -> HTTPS 重定向
site ":80" {
    route "/" {
        redirect "https://{host}{path}" permanent=true
    }
}

// www -> 裸域名
site "www.example.com" {
    route "/" {
        redirect "https://example.com{path}" permanent=true
    }
}

// 旧路径 -> 新路径
site "example.com" {
    route "/blog/old-post" {
        redirect "/articles/new-post" permanent=true
    }
}
```

---

## basic-auth -- 基础认证

实现 HTTP Basic Authentication，使用 bcrypt 哈希存储密码。

> 需要编译时启用 `bcrypt` feature flag。

### 配置

```kdl
route "/" {
    basic-auth {
        user "admin" hash="$2b$12$LJ3m4ys3Lg5Fqm1gHPvateRGMWFn.MRsOZRbMqOo6MjGFOiVOriCa"
        user "reader" hash="$2b$12$abc123..."
    }
    proxy "127.0.0.1:3000"
}
```

### 生成密码哈希

可以使用以下方式生成 bcrypt 哈希：

```bash
# 使用 htpasswd（Apache 工具）
htpasswd -nbBC 12 "" "your-password" | cut -d: -f2

# 使用 Python
python3 -c "import bcrypt; print(bcrypt.hashpw(b'your-password', bcrypt.gensalt(rounds=12)).decode())"
```

### 工作原理

1. Gatel 检查请求的 `Authorization` 头。
2. 如果缺少或格式不正确，返回 `401 Unauthorized`，附带 `WWW-Authenticate: Basic` 头。
3. 解码 Base64 编码的用户名和密码。
4. 使用 bcrypt 验证密码哈希。
5. 验证通过后，请求继续传递给下一个中间件或处理器。

### 注意事项

- 务必使用 HTTPS，Basic Auth 的凭据在 HTTP 下以明文传输。
- bcrypt 的 rounds 参数建议设置为 12 或更高。
- 每次请求都需要进行 bcrypt 验证，这是一个计算密集型操作。在高并发场景下需要注意性能影响。

---

## rate-limit -- 限流

基于令牌桶算法的请求限流。

### 配置

```kdl
route "/api/*" {
    rate-limit window="1m" max=100
    proxy "127.0.0.1:3000"
}
```

### 属性

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `window` | duration | 是 | 时间窗口大小 |
| `max` | int | 是 | 窗口内允许的最大请求数 |

### 时间窗口格式

| 格式 | 说明 |
|---|---|
| `"1s"` | 1 秒 |
| `"30s"` | 30 秒 |
| `"1m"` | 1 分钟 |
| `"5m"` | 5 分钟 |
| `"1h"` | 1 小时 |

### 工作原理

令牌桶算法：

1. 桶的容量为 `max`。
2. 每个时间窗口补充令牌至满。
3. 每个请求消耗一个令牌。
4. 令牌耗尽时，返回 `429 Too Many Requests`。

### 触发限流时的响应

```
HTTP/1.1 429 Too Many Requests
Retry-After: 30
Content-Type: text/plain

Rate limit exceeded. Try again later.
```

### 配置示例

```kdl
// API 限流：每分钟 100 次
route "/api/*" {
    rate-limit window="1m" max=100
    proxy "127.0.0.1:3000"
}

// 登录接口更严格的限流：每分钟 10 次
route "/api/login" {
    rate-limit window="1m" max=10
    proxy "127.0.0.1:3000"
}

// 静态资源更宽松的限流
route "/static/*" {
    rate-limit window="1s" max=50
    file-server
}
```

---

## ip-filter -- IP 过滤

基于客户端 IP 地址的访问控制，支持 CIDR 表示法。

### 配置

```kdl
route "/" {
    ip-filter {
        allow "10.0.0.0/8"
        allow "172.16.0.0/12"
        allow "192.168.0.0/16"
        deny "0.0.0.0/0"
    }
    proxy "127.0.0.1:3000"
}
```

### 规则

- `allow` — 允许匹配的 IP 通过。
- `deny` — 拒绝匹配的 IP。
- 规则按声明顺序匹配，第一个匹配的规则生效。
- 如果没有规则匹配，默认允许。

### CIDR 表示法

| CIDR | 说明 |
|---|---|
| `10.0.0.0/8` | 10.x.x.x（A 类私有地址） |
| `172.16.0.0/12` | 172.16.x.x - 172.31.x.x（B 类私有地址） |
| `192.168.0.0/16` | 192.168.x.x（C 类私有地址） |
| `0.0.0.0/0` | 所有 IPv4 地址 |
| `192.168.1.100/32` | 单个 IP 地址 |

### 示例

```kdl
// 仅允许内网访问
route "/" {
    ip-filter {
        allow "10.0.0.0/8"
        allow "192.168.0.0/16"
        deny "0.0.0.0/0"
    }
    proxy "127.0.0.1:3000"
}

// 封禁特定 IP
route "/" {
    ip-filter {
        deny "1.2.3.4/32"
        deny "5.6.7.0/24"
        allow "0.0.0.0/0"
    }
    proxy "127.0.0.1:3000"
}

// 管理后台仅限办公网络访问
route "/admin/*" {
    ip-filter {
        allow "203.0.113.0/24"
        deny "0.0.0.0/0"
    }
    proxy "127.0.0.1:3001"
}
```

### 被拒绝时的响应

```
HTTP/1.1 403 Forbidden
Content-Type: text/plain

Access denied.
```

---

## cache -- 响应缓存

基于 LRU（Least Recently Used）算法的响应缓存。

### 配置

```kdl
route "/" {
    cache max-entries=1000 max-age="300s"
    proxy "127.0.0.1:3000"
}
```

### 属性

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `max-entries` | int | 是 | 缓存的最大条目数 |
| `max-age` | duration | 是 | 缓存条目的最大存活时间 |

### 工作原理

1. 收到请求时，根据请求的 URL 和相关头信息生成缓存键。
2. 如果缓存命中且未过期，直接返回缓存的响应（不转发到后端）。
3. 如果缓存未命中或已过期，将请求转发到后端。
4. 后端响应后，将响应存入缓存。
5. 当缓存条目数达到 `max-entries` 时，淘汰最久未使用的条目。

### 缓存行为

- 仅缓存 GET 和 HEAD 请求。
- 仅缓存 2xx 响应。
- 尊重 `Cache-Control: no-store` 和 `Cache-Control: no-cache` 头。
- 缓存使用 dashmap 实现，支持并发访问。

### 配置示例

```kdl
// API 短期缓存
route "/api/public/*" {
    cache max-entries=5000 max-age="60s"
    proxy "127.0.0.1:3000"
}

// 静态资源长期缓存
route "/assets/*" {
    cache max-entries=10000 max-age="3600s"
    root "/var/www/static"
    file-server
}
```

---

## templates -- 模板渲染

服务端模板渲染中间件。

### 配置

```kdl
route "/" {
    templates root="/templates"
    file-server
}
```

### 属性

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `root` | string | 是 | 模板文件所在的根目录 |

### 工作原理

1. 当请求的文件是模板文件时，先读取文件内容。
2. 执行模板渲染，替换模板变量。
3. 将渲染后的内容作为响应返回。

---

## 中间件组合示例

### API 网关

```kdl
site "api.example.com" {
    // 公开 API
    route "/v1/public/*" {
        logging
        rate-limit window="1m" max=1000
        encode "gzip" "zstd"
        cache max-entries=5000 max-age="60s"
        proxy {
            upstream "10.0.1.1:3000"
            upstream "10.0.1.2:3000"
            lb "round_robin"
        }
    }

    // 需要认证的 API
    route "/v1/private/*" {
        logging
        rate-limit window="1m" max=500
        basic-auth {
            user "app1" hash="$2b$12$..."
            user "app2" hash="$2b$12$..."
        }
        encode "gzip" "zstd"
        proxy {
            upstream "10.0.1.1:3000"
            upstream "10.0.1.2:3000"
            lb "round_robin"
        }
    }

    // 健康检查端点（无中间件）
    route "/health" {
        respond "ok" status=200
    }
}
```

### 管理后台

```kdl
site "admin.example.com" {
    route "/" {
        logging
        ip-filter {
            allow "10.0.0.0/8"
            allow "203.0.113.0/24"
            deny "0.0.0.0/0"
        }
        basic-auth {
            user "admin" hash="$2b$12$..."
        }
        encode "gzip"
        proxy "127.0.0.1:3001"
    }
}
```

### 静态站点 + 安全头

```kdl
site "www.example.com" {
    route "/" {
        logging
        encode "gzip" "brotli"
        headers {
            response "Strict-Transport-Security" "max-age=31536000; includeSubDomains"
            response "X-Frame-Options" "DENY"
            response "X-Content-Type-Options" "nosniff"
            response "Content-Security-Policy" "default-src 'self'"
        }
        cache max-entries=10000 max-age="3600s"
        root "/var/www/html"
        file-server browse=true
    }
}
```

### 限流层级

```kdl
site "api.example.com" {
    // 全局限流
    route "/*" {
        rate-limit window="1s" max=1000
    }

    // 登录接口严格限流
    route "/login" {
        rate-limit window="1m" max=10
        proxy "127.0.0.1:3000"
    }

    // 普通 API 适中限流
    route "/api/*" {
        rate-limit window="1m" max=200
        proxy "127.0.0.1:3000"
    }
}
```
