# 配置参考

本文档是 Gatel KDL 配置的完整参考手册。Gatel 使用 KDL（KDL Document Language）作为配置格式。

---

## 目录

- [配置格式概述](#配置格式概述)
- [顶层结构](#顶层结构)
- [使用 `import` 拆分配置](#使用-import-拆分配置)
- [global 指令](#global-指令)
- [tls 指令](#tls-指令)
- [site 指令](#site-指令)
- [route 指令](#route-指令)
- [匹配器](#匹配器)
- [处理器](#处理器)
- [中间件](#中间件)
- [stream 指令](#stream-指令)
- [完整配置示例](#完整配置示例)
- [配置验证](#配置验证)

---

## 配置格式概述

KDL 是一种文档语言，语法简洁直观。以下是基本语法规则：

```kdl
// 单行注释

/*
  多行注释
*/

// 节点（node）可以有参数（arguments）和属性（properties）
node "arg1" "arg2" key="value"

// 节点可以有子节点（children）
parent {
    child "value"
}
```

**配置文件默认路径**：`gatel.kdl`（当前工作目录）。

**通过 CLI 指定**：

```bash
gatel run --config /path/to/gatel.kdl
gatel validate --config /path/to/gatel.kdl
```

---

## 顶层结构

Gatel 配置由以下顶层指令组成：

```kdl
// 全局设置（只允许出现在主配置文件中）
global {
    // ...
}

// TLS 全局配置
tls {
    // ...
}

// 站点配置（可定义多个）
site "host:port" {
    // ...
}

// TCP 流代理（可定义多个）
stream {
    // ...
}

// 可复用片段，可以在 site / route 中引用
snippet "name" {
    // ...
}

// 将另一个 KDL 文件原地展开到此处
import "path/to/other.kdl"
```

除 `site` 外，所有顶层指令均为可选。一个最小的配置只需要一个 `site` 指令。出现未识别的顶层节点会产生解析错误。

---

## 使用 `import` 拆分配置

Gatel 仍然**只加载一个主配置文件**（默认 `gatel.kdl`，或通过 `--config` 指定的路径），但你可以在这个主文件中使用 `import` 指令把配置拆分到多个文件：

```kdl
global {
    http ":80"
    https ":443"
    admin ":2019"
}

// 路径相对于包含此 `import` 的文件所在目录。
import "conf.d/api.kdl"
import "conf.d/static.kdl"
import "shared/snippets.kdl"
```

### 规则

- **`import` 只能出现在 KDL 文档的顶层**（与 `global` / `site` 等同级）。
- **路径相对于包含 `import` 的文件所在目录**，而不是进程的当前工作目录。这样 import 可以自然嵌套：`main.kdl` 可以 `import "conf.d/api.kdl"`，而 `conf.d/api.kdl` 中又可以 `import "snippets/auth.kdl"`（相对于 `conf.d/` 解析）。
- **import 按源码出现的顺序原地展开**。例如：
  ```kdl
  import "a.kdl"
  site "main.example.com" { route "/*" { proxy "localhost:3000" } }
  import "b.kdl"
  ```
  处理顺序为：`a.kdl` 的所有节点 → 内联的 `site "main.example.com"` → `b.kdl` 的所有节点。
- **`global` 块只允许出现在主配置文件中**。被 import 的文件中如果出现 `global` 块，会以清晰的错误信息被拒绝。这样可以保证服务器级别的设置（监听地址、日志级别、admin API、优雅关机超时……）始终集中在一个显眼的位置。
- **其它所有顶层块都可以出现在被 import 的文件中**：`tls`、`site`、`stream`、`snippet`，以及嵌套的 `import`。
- **`snippet` 定义是全局可见的**。任何被 import 的文件中定义的 snippet，都可以被合并后配置中的任意 `site` 引用，与 import 顺序无关。
- **循环 import 和菱形 import 是安全的**。加载器通过规范化路径去重：已经加载过的文件会被静默跳过，所以 `a → b → a` 不会无限递归，同一个文件即使通过两条不同路径被 import，也最多加载一次。
- **被 import 的文件不存在时只会发出警告，不会报错**。如果 import 的路径不存在，Gatel 会打印一条 `WARN` 日志然后继续加载。这样你就可以放心地 `import "conf.d/local-overrides.kdl"` 这种可选的 drop-in 文件，即使该文件尚未创建也不会影响配置加载。对于**存在但内容错误**的文件（KDL 语法错误、权限问题、IO 异常等），仍然会以硬错误的形式返回。（注意：**主配置文件本身**仍然是必须的 —— 如果主配置文件不存在，服务器会拒绝启动，除非设置了 `GATEL_*` 环境变量以启用自动配置。）
- **支持 glob 通配符**。包含 `*`、`?` 或 `[...]` 字符类的 import 路径会被当作 glob 模式（基于 [`glob`](https://crates.io/crates/glob) crate 实现），并相对于**包含 import 的文件所在目录**展开。匹配到的文件会**按字母顺序排序**后依次加载，加载顺序与文件系统的目录遍历顺序无关，保证确定性。匹配到零个文件只会打印警告,不会报错 —— 这正是 optional drop-in 目录的惯用写法。模式命中的目录会被跳过,只加载普通文件。

  ```kdl
  // 按字母顺序加载 conf.d/ 下的所有 .kdl 文件
  import "conf.d/*.kdl"

  // 使用数字前缀控制加载顺序（01-api.kdl、02-static.kdl…）
  import "sites/[0-9][0-9]-*.kdl"
  ```

### 热重载

`gatel reload` 和 admin API 的 `POST /config/reload` 都会对主配置文件重新调用 `parse_config_file`，因此对被 import 文件的修改在热重载时也会被正确地加载。Unix 下的 `SIGHUP` 信号同理。

### 校验

`gatel validate --config /path/to/gatel.kdl` 同样会解析 import，所以你可以在重载运行中的服务器之前，先校验拆分后的配置是否正确：

```bash
gatel validate --config /etc/gatel/gatel.kdl
```

### 示例：`conf.d` 目录结构

```
/etc/gatel/
├── gatel.kdl          # 主文件 —— global 只能写在这里
└── conf.d/
    ├── api.kdl        # site "api.example.com" { … }
    ├── static.kdl     # site "www.example.com" { … }
    └── snippets.kdl   # snippet "common-headers" { … }
```

`gatel.kdl`：

```kdl
global {
    http ":80"
    https ":443"
    admin ":2019"
    log level="info" format="json"
}

tls {
    acme {
        email "admin@example.com"
    }
}

import "conf.d/snippets.kdl"
import "conf.d/api.kdl"
import "conf.d/static.kdl"
```

---

## global 指令

`global` 指令定义服务器级别的全局设置。

### admin

管理 API 监听地址。

```kdl
global {
    admin ":2019"
}
```

管理 API 提供以下端点：

| 端点 | 方法 | 说明 |
|---|---|---|
| `/config` | GET | 获取当前配置 |
| `/config/reload` | POST | 热重载配置 |
| `/health` | GET | 健康状态 |
| `/upstreams` | GET | 上游后端状态 |
| `/metrics` | GET | Prometheus 指标 |

### log

日志配置。

```kdl
global {
    log level="info" format="json"
}
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `level` | string | `"info"` | 日志级别：`trace`、`debug`、`info`、`warn`、`error` |
| `format` | string | `"pretty"` | 输出格式：`pretty`（人类可读）或 `json` |

### grace-period

优雅关闭的宽限期。超过此时间后，未完成的连接将被强制关闭。

```kdl
global {
    grace-period "30s"
}
```

支持的时间单位：`s`（秒）、`m`（分钟）、`h`（小时）。

### http / https

自定义 HTTP 和 HTTPS 监听地址。

```kdl
global {
    http ":80"
    https ":443"
}
```

默认值：HTTP 监听 `:80`，HTTPS 监听 `:443`。

### http3

启用 HTTP/3 (QUIC) 支持。需要编译时启用 `http3` feature flag。

```kdl
global {
    http3 true
}
```

### proxy-protocol

启用 PROXY Protocol 支持（v1 文本格式和 v2 二进制格式）。

```kdl
global {
    proxy-protocol true
}
```

启用后，Gatel 会解析入站连接的 PROXY Protocol 头，提取真实客户端 IP。

### 完整示例

```kdl
global {
    admin ":2019"
    log level="info" format="json"
    grace-period "30s"
    http ":80"
    https ":443"
    http3 true
    proxy-protocol true
}
```

---

## tls 指令

`tls` 顶层指令定义全局 TLS 设置，包括 ACME 自动证书、客户端认证和按需 TLS。

### acme

自动证书管理（ACME 协议）。

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}
```

**子节点**：

| 节点 | 说明 |
|---|---|
| `email` | ACME 账号邮箱（必填） |
| `ca` | CA 提供商：`letsencrypt`、`zerossl` |
| `challenge` | 挑战类型：`http-01` |

### client-auth

mTLS 客户端证书认证。

```kdl
tls {
    client-auth required=true {
        ca-cert "/etc/gatel/client-ca.pem"
    }
}
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `required` | bool | `false` | 是否强制要求客户端证书 |

**子节点**：

| 节点 | 说明 |
|---|---|
| `ca-cert` | 用于验证客户端证书的 CA 证书路径 |

### on-demand

按需 TLS — 在首次 TLS 握手时自动为域名签发证书。

```kdl
tls {
    on-demand ask="https://auth.example.com/check" rate-limit=10
}
```

**属性**：

| 属性 | 类型 | 说明 |
|---|---|---|
| `ask` | string | 验证 URL，Gatel 会发送 GET 请求，2xx 响应表示允许签发 |
| `rate-limit` | int | 每分钟允许签发的最大证书数 |

更多 TLS 配置细节请参阅 [TLS 与 ACME](tls-and-acme.md)。

---

## site 指令

`site` 指令定义一个站点。一份配置中可以有多个 `site` 指令。

```kdl
site "host:port" {
    // 站点级 TLS
    tls { ... }

    // 路由规则
    route "/path/*" { ... }
}
```

### 站点地址格式

```kdl
// 主机名 + 端口
site "example.com:443" { }

// 仅主机名（HTTPS 默认 443，HTTP 默认 80）
site "example.com" { }

// 仅端口（监听所有接口）
site ":8080" { }

// localhost
site "localhost:8080" { }
```

### 站点级 TLS

每个站点可以配置独立的 TLS 证书：

```kdl
site "example.com" {
    tls {
        cert "/etc/gatel/certs/example.com.pem"
        key "/etc/gatel/certs/example.com-key.pem"
    }
}
```

如果未配置站点级 TLS，且全局 `tls` 指令中配置了 ACME，Gatel 将自动为真实域名签发证书。

### 多站点

```kdl
site "api.example.com" {
    route "/" {
        proxy "127.0.0.1:3000"
    }
}

site "www.example.com" {
    route "/" {
        root "/var/www/html"
        file-server
    }
}

site "admin.example.com" {
    route "/" {
        basic-auth {
            user "admin" hash="$2b$12$..."
        }
        proxy "127.0.0.1:3001"
    }
}
```

---

## route 指令

`route` 指令定义站点内的路由规则。路由按声明顺序匹配，第一个匹配的路由处理请求。

```kdl
site "example.com" {
    route "/api/*" {
        // 匹配器、中间件、处理器
    }

    route "/" {
        // 默认路由
    }
}
```

### 路径模式

| 模式 | 说明 |
|---|---|
| `"/"` | 匹配所有路径 |
| `"/api"` | 精确匹配 `/api` |
| `"/api/*"` | 匹配 `/api/` 下的所有路径 |
| `"/images/*.jpg"` | 匹配 `/images/` 下的所有 `.jpg` 文件 |

### 路由组成

一个 route 内可以包含三类指令：

1. **匹配器** (`match`) — 额外的匹配条件
2. **中间件** — 请求/响应处理管道
3. **处理器** — 最终的请求处理逻辑

```kdl
route "/api/*" {
    // 匹配器
    match method="GET,POST"
    match header="Authorization" pattern="Bearer *"

    // 中间件
    rate-limit window="1m" max=100
    encode "gzip"

    // 处理器
    proxy "127.0.0.1:3000"
}
```

---

## 匹配器

匹配器在路径匹配的基础上，提供额外的请求匹配条件。同一 route 中的多个匹配器之间是 AND 关系。

### method — HTTP 方法匹配

```kdl
match method="GET"
match method="GET,POST,PUT"
```

### header — 请求头匹配

```kdl
match header="Content-Type" pattern="application/json*"
match header="X-Custom-Header" pattern="foo*"
```

### query — 查询参数匹配

```kdl
match query="key" value="val"
```

### remote-ip — 客户端 IP 匹配

支持 CIDR 表示法。

```kdl
match remote-ip="192.168.0.0/16"
match remote-ip="10.0.0.0/8"
```

### protocol — 协议匹配

```kdl
match protocol="https"
match protocol="http"
```

### expression — 表达式匹配

支持组合条件的表达式语法。

```kdl
match expression="{method} == GET && {path} ~ /api/*"
```

可用变量：

| 变量 | 说明 |
|---|---|
| `{method}` | HTTP 方法 |
| `{path}` | 请求路径 |
| `{host}` | 主机名 |
| `{remote_ip}` | 客户端 IP |
| `{protocol}` | 协议（http/https） |

运算符：

| 运算符 | 说明 |
|---|---|
| `==` | 等于 |
| `!=` | 不等于 |
| `~` | 通配符匹配 |
| `&&` | 逻辑与 |
| `\|\|` | 逻辑或 |

### not — 否定匹配

```kdl
match not {
    match remote-ip="10.0.0.0/8"
}
```

### 组合示例

```kdl
route "/api/*" {
    // 仅匹配 HTTPS + GET/POST + 来自内网的请求
    match protocol="https"
    match method="GET,POST"
    match remote-ip="10.0.0.0/8"

    proxy "127.0.0.1:3000"
}
```

---

## 处理器

每个路由必须有且仅有一个终端处理器。

### proxy — 反向代理

简单形式：

```kdl
proxy "127.0.0.1:3000"
```

完整形式：

```kdl
proxy {
    upstream "127.0.0.1:3001" weight=3
    upstream "127.0.0.1:3002" weight=1
    lb "round_robin"
    retries 2
    health-check path="/health" interval="10s" timeout="3s" threshold=3
    passive-health fail-duration="30s" max-fails=5 unhealthy-status="502,503,504"
    header-up "X-Real-IP" "{remote_ip}"
    header-down "-Server"
    dns-upstream interval="60s"
}
```

详细说明请参阅 [反向代理](reverse-proxy.md)。

### fastcgi — FastCGI 代理

```kdl
fastcgi "127.0.0.1:9000" {
    root "/var/www"
    split ".php"
    index "index.php"
    env "DB_HOST" "localhost"
    env "DB_PORT" "3306"
}
```

### root + file-server — 静态文件服务

```kdl
root "/var/www/html"
file-server browse=true
```

### redirect — HTTP 重定向

```kdl
redirect "https://example.com{path}" permanent=true
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `permanent` | bool | `false` | `true` 为 301，`false` 为 302 |

### respond — 固定响应

```kdl
respond "Hello, World!" status=200
```

---

## 中间件

中间件在处理器之前处理请求，按声明顺序执行。

### rate-limit — 限流

```kdl
rate-limit window="1m" max=100
```

### encode — 压缩

```kdl
encode "gzip" "zstd" "brotli"
```

### basic-auth — 基础认证

```kdl
basic-auth {
    user "admin" hash="$2b$12$..."
    user "reader" hash="$2b$12$..."
}
```

### cache — 响应缓存

```kdl
cache max-entries=1000 max-age="300s"
```

### templates — 模板渲染

```kdl
templates root="/templates"
```

### headers — 请求头/响应头操作

参见 [反向代理](reverse-proxy.md) 中的 `header-up` 和 `header-down`。

### rewrite — URL 重写

```kdl
rewrite "/old-path" "/new-path"
```

### ip-filter — IP 过滤

```kdl
ip-filter {
    allow "10.0.0.0/8"
    allow "192.168.0.0/16"
    deny "0.0.0.0/0"
}
```

### logging — 访问日志

```kdl
logging
```

中间件的完整说明请参阅 [中间件参考](middleware.md)。

---

## stream 指令

`stream` 指令用于配置 L4 TCP 流代理（非 HTTP）。

```kdl
stream {
    listen ":3306" {
        proxy "mysql-server:3306"
    }

    listen ":6379" {
        proxy "redis-server:6379"
    }
}
```

这会在 TCP 层进行双向数据转发，不解析应用层协议。适用于数据库、Redis 等 TCP 服务的代理。

详细说明请参阅 [高级功能](advanced-features.md)。

---

## 完整配置示例

以下是一个包含所有主要功能的完整配置示例：

```kdl
// 全局设置
global {
    admin ":2019"
    log level="info" format="json"
    grace-period "30s"
    http ":80"
    https ":443"
}

// 全局 TLS（ACME 自动证书）
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

// API 服务
site "api.example.com" {
    route "/v1/*" {
        rate-limit window="1m" max=1000
        encode "gzip" "zstd"

        proxy {
            upstream "10.0.1.1:3000" weight=3
            upstream "10.0.1.2:3000" weight=2
            upstream "10.0.1.3:3000" weight=1
            lb "weighted_round_robin"
            retries 2
            health-check path="/health" interval="10s" timeout="3s" threshold=3
            passive-health fail-duration="30s" max-fails=5 unhealthy-status="502,503,504"
            header-up "X-Real-IP" "{remote_ip}"
            header-up "X-Forwarded-Proto" "{protocol}"
        }
    }

    route "/health" {
        respond "ok" status=200
    }
}

// 前端静态站点
site "www.example.com" {
    route "/assets/*" {
        encode "gzip" "brotli"
        cache max-entries=5000 max-age="3600s"
        root "/var/www/static"
        file-server
    }

    route "/" {
        root "/var/www/html"
        file-server
    }
}

// 管理后台
site "admin.example.com" {
    route "/" {
        ip-filter {
            allow "10.0.0.0/8"
            deny "0.0.0.0/0"
        }
        basic-auth {
            user "admin" hash="$2b$12$LJ3m4ys3Lg..."
        }
        proxy "127.0.0.1:3001"
    }
}

// PHP 应用
site "php.example.com" {
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

// HTTP -> HTTPS 重定向
site ":80" {
    route "/" {
        redirect "https://{host}{path}" permanent=true
    }
}

// TCP 流代理
stream {
    listen ":3306" {
        proxy "mysql-primary:3306"
    }
}
```

---

## 配置验证

在部署前，务必使用 `validate` 命令检查配置：

```bash
gatel validate --config gatel.kdl
```

如果配置有效，输出 "Configuration is valid"。如果有错误，会显示具体的错误信息和位置。

常见配置错误：

- KDL 语法错误（括号不匹配、缺少引号）
- 未知的指令名称
- 缺少必填参数
- 端口冲突（多个 site 监听相同地址）
- 证书文件路径不存在（使用手动 TLS 时）
