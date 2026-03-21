# TLS 与 ACME

本文档详细介绍 Gatel 的 TLS 配置，包括自动证书管理 (ACME)、手动证书、双向 TLS (mTLS) 和按需 TLS。

---

## 目录

- [TLS 概述](#tls-概述)
- [自动 HTTPS (ACME)](#自动-https-acme)
- [手动证书](#手动证书)
- [双向 TLS (mTLS)](#双向-tls-mtls)
- [按需 TLS (On-Demand TLS)](#按需-tls-on-demand-tls)
- [TLS 协议细节](#tls-协议细节)
- [证书管理最佳实践](#证书管理最佳实践)
- [故障排查](#故障排查)

---

## TLS 概述

Gatel 使用 tokio-rustls（基于 rustls）作为 TLS 实现，通过 certon 库管理自动证书。

**TLS 支持层级**：

| 特性 | 说明 |
|---|---|
| TLS 1.2 | 支持 |
| TLS 1.3 | 支持（默认优先） |
| ALPN | 自动协商 h2 和 http/1.1 |
| SNI | 支持多站点证书选择 |
| OCSP Stapling | 通过 certon 支持 |

**证书配置方式**：

1. **自动 ACME** — 自动从 CA 获取和续期证书（推荐）。
2. **手动证书** — 指定 PEM 格式的证书和私钥文件。
3. **按需 TLS** — 首次握手时自动签发证书。

---

## 自动 HTTPS (ACME)

ACME（Automatic Certificate Management Environment）协议允许 Gatel 自动从证书颁发机构获取 TLS 证书。

### 基础配置

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

配置全局 ACME 后，所有使用真实域名的 site 都会自动获取证书。

### ACME 参数

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `email` | string | 是 | 账号邮箱，用于证书过期通知和账号恢复 |
| `ca` | string | 否 | CA 提供商，默认 `letsencrypt` |
| `challenge` | string | 否 | 挑战类型，默认 `http-01` |

### 支持的 CA

| CA | 配置值 | 说明 |
|---|---|---|
| Let's Encrypt | `"letsencrypt"` | 免费，使用最广泛 |
| ZeroSSL | `"zerossl"` | 免费，备选 CA |

### HTTP-01 挑战

HTTP-01 是最常用的域名验证方式：

1. Gatel 向 CA 请求签发证书。
2. CA 返回一个挑战令牌。
3. CA 通过 HTTP 访问 `http://your-domain/.well-known/acme-challenge/<token>` 验证域名控制权。
4. Gatel 自动响应挑战请求。
5. 验证通过后，CA 签发证书。

**前提条件**：

- 域名必须已解析到运行 Gatel 的服务器。
- Gatel 必须能在端口 80 上接收 HTTP 请求（CA 验证使用 HTTP）。
- 服务器必须能访问外网（与 CA 通信）。

### 证书自动续期

certon 自动处理证书续期：

- 在证书过期前 30 天开始尝试续期。
- 续期在后台自动进行，不影响正常服务。
- 如果续期失败，会持续重试并记录错误日志。
- 新证书生效后，通过热切换无缝替换旧证书。

### 多站点 ACME

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

// 每个站点自动获取各自的证书
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
        proxy "127.0.0.1:3001"
    }
}
```

---

## 手动证书

当需要使用特定证书（如企业内部 CA 签发的证书）时，可以手动指定。

### 站点级配置

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

### 参数

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cert` | string | 是 | PEM 格式的证书文件路径（包含完整证书链） |
| `key` | string | 是 | PEM 格式的私钥文件路径 |

### 证书文件格式

证书文件应包含完整的证书链，从服务器证书开始，到中间 CA 证书结束：

```
-----BEGIN CERTIFICATE-----
（服务器证书）
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
（中间 CA 证书）
-----END CERTIFICATE-----
```

私钥文件：

```
-----BEGIN PRIVATE KEY-----
（私钥内容）
-----END PRIVATE KEY-----
```

### 混合模式

可以同时使用 ACME 和手动证书。部分站点使用自动证书，部分使用手动证书：

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}

// 这个站点使用 ACME 自动证书
site "public.example.com" {
    route "/" {
        proxy "127.0.0.1:3000"
    }
}

// 这个站点使用手动证书
site "internal.example.com" {
    tls {
        cert "/etc/gatel/certs/internal.pem"
        key "/etc/gatel/certs/internal-key.pem"
    }

    route "/" {
        proxy "127.0.0.1:3001"
    }
}
```

配置了站点级 `tls` 的站点不会使用全局 ACME。

### 证书热更新

当手动证书文件更新后，可以通过热重载使新证书生效：

```bash
# 更新证书文件后
gatel reload
# 或
curl -X POST http://localhost:2019/config/reload
```

---

## 双向 TLS (mTLS)

mTLS（Mutual TLS）要求客户端也提供证书，实现双向身份认证。适用于服务间通信、零信任网络等场景。

### 配置

```kdl
tls {
    client-auth required=true {
        ca-cert "/etc/gatel/client-ca.pem"
    }
}

site "secure-api.example.com" {
    route "/" {
        proxy "127.0.0.1:3000"
    }
}
```

### 参数

| 参数 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `required` | bool | `false` | `true`：强制要求客户端证书；`false`：可选（请求但不强制） |

### 子节点

| 节点 | 说明 |
|---|---|
| `ca-cert` | 用于验证客户端证书的 CA 证书路径（PEM 格式） |

### 工作原理

1. TLS 握手时，Gatel 发送 `CertificateRequest` 消息给客户端。
2. 客户端发送其证书（和证书链）。
3. Gatel 使用配置的 CA 证书验证客户端证书的签名链。
4. 如果 `required=true` 且验证失败，拒绝连接。
5. 如果 `required=false` 且客户端未提供证书，允许连接继续。

### 客户端连接示例

```bash
# 使用 curl 连接 mTLS 服务
curl --cert client.pem --key client-key.pem https://secure-api.example.com/

# 使用 openssl 测试
openssl s_client -connect secure-api.example.com:443 \
    -cert client.pem -key client-key.pem
```

### 生成客户端证书

```bash
# 1. 创建客户端 CA
openssl req -x509 -newkey rsa:4096 -keyout client-ca-key.pem -out client-ca.pem \
    -days 3650 -nodes -subj "/CN=Client CA"

# 2. 创建客户端密钥和证书签名请求
openssl req -newkey rsa:2048 -keyout client-key.pem -out client.csr \
    -nodes -subj "/CN=my-client"

# 3. 用 CA 签发客户端证书
openssl x509 -req -in client.csr -CA client-ca.pem -CAkey client-ca-key.pem \
    -CAcreateserial -out client.pem -days 365
```

将 `client-ca.pem` 配置为 Gatel 的 `ca-cert`，客户端使用 `client.pem` 和 `client-key.pem`。

### mTLS + ACME

mTLS 可以与 ACME 服务器证书同时使用：

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }

    client-auth required=true {
        ca-cert "/etc/gatel/client-ca.pem"
    }
}
```

这种配置下，服务器证书由 ACME 自动管理，客户端证书由自有 CA 签发。

---

## 按需 TLS (On-Demand TLS)

按需 TLS 允许 Gatel 在首次收到某个域名的 TLS 握手请求时，实时签发证书。适用于管理大量动态域名的场景。

### 配置

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }

    on-demand ask="https://auth.example.com/check" rate-limit=10
}
```

### 参数

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ask` | string | 建议配置 | 域名验证 URL |
| `rate-limit` | int | 建议配置 | 每分钟最大证书签发数 |

### 工作原理

1. 客户端发起 TLS 握手，SNI 中包含域名 `custom.example.com`。
2. Gatel 检查是否已有该域名的证书。
3. 如果没有，向 `ask` URL 发送验证请求：`GET https://auth.example.com/check?domain=custom.example.com`。
4. 如果验证服务返回 2xx 状态码，允许签发。
5. 通过 ACME 为该域名签发证书。
6. 完成 TLS 握手，处理请求。
7. 证书被缓存，后续请求直接使用。

### ask 验证端点

验证端点应该检查请求的域名是否合法：

```
GET https://auth.example.com/check?domain=custom.example.com

返回 200 → 允许签发
返回 403 → 拒绝签发
```

验证逻辑示例（伪代码）：

```python
def check_domain(domain):
    # 检查域名是否在数据库中注册
    if domain in registered_domains:
        return 200
    # 检查域名是否符合通配符模式
    if domain.endswith(".myplatform.com"):
        return 200
    return 403
```

### 安全注意事项

按需 TLS 如果不加限制，可能被滥用来消耗 ACME 配额。务必配置：

1. **`ask` URL** — 验证域名的合法性，拒绝未授权的域名。
2. **`rate-limit`** — 限制签发速率，防止突发大量签发请求。

```kdl
tls {
    acme {
        email "admin@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }

    // 始终配置 ask 和 rate-limit
    on-demand ask="https://auth.example.com/check" rate-limit=5
}
```

### 适用场景

- SaaS 平台允许用户绑定自定义域名。
- CDN 服务为客户域名自动签发证书。
- 多租户平台的动态域名管理。

---

## TLS 协议细节

### 支持的协议版本

Gatel 通过 rustls 支持：

| 版本 | 支持 | 说明 |
|---|---|---|
| TLS 1.0 | 否 | 已废弃，不安全 |
| TLS 1.1 | 否 | 已废弃，不安全 |
| TLS 1.2 | 是 | 向后兼容 |
| TLS 1.3 | 是 | 默认优先，推荐 |

### ALPN 协商

Gatel 通过 ALPN (Application-Layer Protocol Negotiation) 自动协商 HTTP 协议版本：

- 优先协商 `h2`（HTTP/2）。
- 回退到 `http/1.1`（HTTP/1.1）。

### SNI 路由

当多个站点配置在同一个 HTTPS 端口上时，Gatel 使用 SNI (Server Name Indication) 选择对应的证书和站点配置：

```kdl
// 两个站点共享 443 端口，通过 SNI 区分
site "api.example.com" {
    tls { cert "api.pem"; key "api-key.pem" }
    route "/" { proxy "127.0.0.1:3000" }
}

site "www.example.com" {
    tls { cert "www.pem"; key "www-key.pem" }
    route "/" { file-server }
}
```

---

## 证书管理最佳实践

### 1. 优先使用 ACME

自动证书管理消除了手动续期的操作负担，减少了因证书过期导致服务中断的风险。

```kdl
tls {
    acme {
        email "ops-team@example.com"
        ca "letsencrypt"
        challenge "http-01"
    }
}
```

### 2. 配置证书过期监控

即使使用 ACME 自动续期，也建议监控证书状态：

```bash
# 通过管理 API 检查证书信息
curl http://localhost:2019/health
```

### 3. 使用强密码套件

rustls 默认仅启用安全的密码套件，无需手动配置。不支持已知不安全的算法（如 RC4、3DES）。

### 4. 启用 HSTS

在 headers 中间件中配置 HSTS：

```kdl
route "/" {
    headers {
        response "Strict-Transport-Security" "max-age=31536000; includeSubDomains; preload"
    }
    proxy "127.0.0.1:3000"
}
```

### 5. HTTP -> HTTPS 重定向

确保所有 HTTP 请求重定向到 HTTPS：

```kdl
site ":80" {
    route "/" {
        redirect "https://{host}{path}" permanent=true
    }
}
```

---

## 故障排查

### 证书签发失败

**症状**：ACME 证书签发失败，日志中出现相关错误。

**排查步骤**：

1. 确认域名已正确解析到服务器 IP：
   ```bash
   dig +short example.com
   ```

2. 确认端口 80 可从外网访问（HTTP-01 挑战需要）：
   ```bash
   curl -v http://example.com/.well-known/acme-challenge/test
   ```

3. 确认没有防火墙阻止入站 HTTP 请求。

4. 检查 Gatel 日志中的详细错误信息：
   ```kdl
   global {
       log level="debug" format="pretty"
   }
   ```

### mTLS 连接失败

**症状**：客户端无法建立连接，或收到 TLS 错误。

**排查步骤**：

1. 验证客户端证书是否由配置的 CA 签发：
   ```bash
   openssl verify -CAfile client-ca.pem client.pem
   ```

2. 检查客户端证书是否过期：
   ```bash
   openssl x509 -in client.pem -noout -dates
   ```

3. 确认 `ca-cert` 路径正确，文件可读。

### TLS 握手超时

**症状**：客户端连接超时，无法完成 TLS 握手。

**排查步骤**：

1. 确认服务器端口可达：
   ```bash
   telnet example.com 443
   ```

2. 测试 TLS 握手：
   ```bash
   openssl s_client -connect example.com:443 -servername example.com
   ```

3. 检查是否有 SNI 不匹配的问题。
