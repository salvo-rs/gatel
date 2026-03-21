# 反向代理

本文档详细介绍 Gatel 的反向代理功能，包括负载均衡、健康检查、重试机制和请求头操作。

---

## 目录

- [基础代理](#基础代理)
- [多上游与负载均衡](#多上游与负载均衡)
- [负载均衡策略详解](#负载均衡策略详解)
- [健康检查](#健康检查)
- [重试机制](#重试机制)
- [请求头操作](#请求头操作)
- [DNS 上游](#dns-上游)
- [WebSocket 代理](#websocket-代理)
- [超时配置](#超时配置)
- [完整配置参考](#完整配置参考)

---

## 基础代理

最简单的反向代理只需要一行配置：

```kdl
site "localhost:8080" {
    route "/" {
        proxy "127.0.0.1:3000"
    }
}
```

这会将所有请求转发到 `127.0.0.1:3000`，并将响应返回给客户端。

### 路径前缀代理

```kdl
site "localhost:8080" {
    route "/api/*" {
        proxy "127.0.0.1:3000"
    }

    route "/static/*" {
        root "/var/www/static"
        file-server
    }
}
```

---

## 多上游与负载均衡

当配置多个上游时，Gatel 使用负载均衡器分配请求：

```kdl
route "/" {
    proxy {
        upstream "10.0.1.1:3000"
        upstream "10.0.1.2:3000"
        upstream "10.0.1.3:3000"
        lb "round_robin"
    }
}
```

### upstream 节点

每个 `upstream` 定义一个后端服务器。

```kdl
upstream "host:port" weight=N
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `weight` | int | `1` | 权重值，仅 `weighted_round_robin` 策略使用 |

---

## 负载均衡策略详解

通过 `lb` 节点指定负载均衡策略。Gatel 提供 9 种策略：

### round_robin — 轮询

```kdl
lb "round_robin"
```

依次将请求分配给每个健康后端。这是最常用的策略，适合后端性能一致的场景。

**工作原理**：维护一个计数器，每次请求递增，对健康后端数量取模。

### random — 随机

```kdl
lb "random"
```

随机选择一个健康后端。在后端数量较多时，统计上接近均匀分布。

### weighted_round_robin — 加权轮询

```kdl
proxy {
    upstream "10.0.1.1:3000" weight=5
    upstream "10.0.1.2:3000" weight=3
    upstream "10.0.1.3:3000" weight=2
    lb "weighted_round_robin"
}
```

采用 nginx 风格的平滑加权轮询算法。权重越高的后端分配到的请求越多。

**工作原理**：每轮为每个后端增加其权重值，选择当前权重最高的后端，被选中的后端减去总权重。这保证了请求在时间维度上的均匀分布。

**适用场景**：后端服务器性能不一致时，按能力分配流量。

**示例**：上面的配置中，10 个请求大约按 5:3:2 的比例分配。

### ip_hash — IP 哈希

```kdl
lb "ip_hash"
```

基于客户端 IP 地址计算一致性哈希，同一 IP 的请求总是路由到相同的后端。

**适用场景**：需要会话保持（session affinity）但不想使用 cookie 的场景。

**注意事项**：
- 如果启用了 PROXY Protocol，使用的是真实客户端 IP。
- 当后端数量变化时，部分客户端的映射关系会改变。

### least_conn — 最少连接

```kdl
lb "least_conn"
```

选择当前活跃连接数最少的健康后端。

**适用场景**：后端请求处理时间差异较大时（如长连接和短连接混合），能更好地平衡负载。

### uri_hash — URI 哈希

```kdl
lb "uri_hash"
```

基于请求 URI 计算一致性哈希，相同 URI 的请求总是路由到相同的后端。

**适用场景**：后端有缓存层时，相同 URI 命中同一后端，可提高缓存命中率。

### header_hash — 请求头哈希

```kdl
lb "header_hash" header="X-User-ID"
```

基于指定请求头的值计算哈希。

**属性**：

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `header` | string | 是 | 用于哈希计算的请求头名称 |

**适用场景**：需要基于特定标识（如用户 ID、租户 ID）进行会话保持。

### cookie_hash — Cookie 哈希

```kdl
lb "cookie_hash" cookie="session_id"
```

基于指定 Cookie 的值计算哈希。

**属性**：

| 属性 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cookie` | string | 是 | 用于哈希计算的 Cookie 名称 |

**适用场景**：基于会话 Cookie 实现会话保持，是最精确的会话亲和方案。

### first — 首选

```kdl
lb "first"
```

始终选择列表中第一个健康后端。如果第一个后端不可用，选择第二个，依此类推。

**适用场景**：主备（Active-Standby）模式，正常情况下所有流量由主后端处理，仅在主后端故障时切换到备用后端。

### 策略对比

| 策略 | 会话保持 | 均匀分布 | 适用场景 |
|---|---|---|---|
| round_robin | 否 | 是 | 通用，后端性能一致 |
| random | 否 | 统计均匀 | 通用 |
| weighted_round_robin | 否 | 按权重 | 后端性能不一致 |
| ip_hash | 按 IP | 取决于 IP 分布 | 会话保持（无 cookie） |
| least_conn | 否 | 自适应 | 请求处理时间差异大 |
| uri_hash | 按 URI | 取决于 URI 分布 | 缓存友好 |
| header_hash | 按请求头 | 取决于请求头分布 | 自定义亲和 |
| cookie_hash | 按 Cookie | 取决于 Cookie 分布 | 精确会话保持 |
| first | 否 | 否 | 主备模式 |

---

## 健康检查

Gatel 支持主动健康检查和被动健康检查，两者可以同时使用。

### 主动健康检查

定期向后端发送 HTTP 请求，根据响应判断后端是否健康。

```kdl
proxy {
    upstream "10.0.1.1:3000"
    upstream "10.0.1.2:3000"

    health-check path="/health" interval="10s" timeout="3s" threshold=3
}
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `path` | string | `"/"` | 健康检查请求路径 |
| `interval` | duration | `"30s"` | 检查间隔 |
| `timeout` | duration | `"5s"` | 单次检查超时 |
| `threshold` | int | `3` | 连续失败次数达到此值后标记为不健康 |

**工作原理**：

1. Gatel 按 `interval` 间隔向每个上游发送 HTTP GET 请求。
2. 如果收到 2xx 响应，重置失败计数。
3. 如果超时或收到非 2xx 响应，增加失败计数。
4. 连续失败次数达到 `threshold` 时，标记后端为不健康。
5. 不健康的后端不再接收新请求，但继续接受健康检查。
6. 当不健康后端再次通过检查，自动恢复为健康状态。

### 被动健康检查

基于正常流量的响应来判断后端健康状态，无需额外的探测请求。

```kdl
proxy {
    upstream "10.0.1.1:3000"
    upstream "10.0.1.2:3000"

    passive-health fail-duration="30s" max-fails=5 unhealthy-status="502,503,504"
}
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `fail-duration` | duration | `"30s"` | 失败计数的时间窗口 |
| `max-fails` | int | `5` | 窗口内最大失败次数 |
| `unhealthy-status` | string | `"502,503,504"` | 视为失败的 HTTP 状态码（逗号分隔） |

**工作原理**：

1. 监控转发到后端的正常请求的响应。
2. 如果响应状态码在 `unhealthy-status` 列表中，记录一次失败。
3. 在 `fail-duration` 时间窗口内，累计失败次数达到 `max-fails` 时，标记后端为不健康。
4. 经过 `fail-duration` 时间后，后端自动恢复为健康状态并重新接收流量。

### 组合使用

主动和被动健康检查可以同时使用，提供更全面的健康监控：

```kdl
proxy {
    upstream "10.0.1.1:3000"
    upstream "10.0.1.2:3000"

    // 主动检查：每 10 秒探测一次
    health-check path="/health" interval="10s" timeout="3s" threshold=3

    // 被动检查：30 秒内 5 次 5xx 则标记不健康
    passive-health fail-duration="30s" max-fails=5 unhealthy-status="502,503,504"
}
```

在这种配置下：
- 被动检查能快速发现正在处理流量时出现的问题。
- 主动检查能发现后端虽然不在接收流量但已恢复的情况。

---

## 重试机制

当请求到上游失败时，Gatel 可以自动重试其他后端：

```kdl
proxy {
    upstream "10.0.1.1:3000"
    upstream "10.0.1.2:3000"
    upstream "10.0.1.3:3000"
    retries 2
}
```

**说明**：

- `retries 2` 表示最多重试 2 次（加上首次请求，总共最多 3 次尝试）。
- 重试时会选择不同的后端（避免重试同一个失败的后端）。
- 仅在连接失败或超时时触发重试，不会在收到后端响应后重试。

---

## 请求头操作

### header-up — 修改发往上游的请求头

```kdl
proxy {
    upstream "10.0.1.1:3000"

    // 添加或设置请求头
    header-up "X-Real-IP" "{remote_ip}"
    header-up "X-Forwarded-For" "{remote_ip}"
    header-up "X-Forwarded-Proto" "{protocol}"
    header-up "X-Request-ID" "{request_id}"

    // 删除请求头（前缀 - 号）
    header-up "-Accept-Encoding"
}
```

### header-down — 修改返回客户端的响应头

```kdl
proxy {
    upstream "10.0.1.1:3000"

    // 添加或设置响应头
    header-down "X-Served-By" "gatel"
    header-down "Strict-Transport-Security" "max-age=31536000; includeSubDomains"

    // 删除响应头（前缀 - 号）
    header-down "-Server"
    header-down "-X-Powered-By"
}
```

### 可用变量

在 `header-up` 的值中可以使用以下变量：

| 变量 | 说明 |
|---|---|
| `{remote_ip}` | 客户端 IP 地址 |
| `{protocol}` | 请求协议（http/https） |
| `{host}` | 请求主机名 |
| `{method}` | HTTP 方法 |
| `{path}` | 请求路径 |
| `{request_id}` | 请求唯一 ID |

### 典型配置

```kdl
proxy {
    upstream "10.0.1.1:3000"

    // 传递真实客户端信息
    header-up "X-Real-IP" "{remote_ip}"
    header-up "X-Forwarded-For" "{remote_ip}"
    header-up "X-Forwarded-Proto" "{protocol}"
    header-up "X-Forwarded-Host" "{host}"

    // 安全相关响应头
    header-down "X-Content-Type-Options" "nosniff"
    header-down "X-Frame-Options" "DENY"
    header-down "Strict-Transport-Security" "max-age=31536000"

    // 隐藏后端信息
    header-down "-Server"
    header-down "-X-Powered-By"
}
```

---

## DNS 上游

当上游使用域名而非 IP 地址时，Gatel 可以定期刷新 DNS 解析结果：

```kdl
proxy {
    upstream "backend.internal:3000"

    dns-upstream interval="60s"
}
```

**属性**：

| 属性 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `interval` | duration | `"60s"` | DNS 解析刷新间隔 |

**工作原理**：

1. 启动时对上游域名执行 A/AAAA DNS 查询。
2. 按 `interval` 间隔定期重新解析。
3. 如果解析结果变化，自动更新上游列表。

**适用场景**：

- 上游服务使用 Kubernetes Service DNS。
- 上游服务使用动态 IP（如自动扩缩容）。
- 上游服务使用 DNS 负载均衡。

---

## WebSocket 代理

Gatel 自动识别并透明代理 WebSocket 连接，无需额外配置：

```kdl
site "ws.example.com" {
    route "/ws" {
        proxy "127.0.0.1:8001"
    }
}
```

Gatel 会检测 `Upgrade: websocket` 头，自动将连接升级为 WebSocket，并在客户端和上游之间双向转发数据。

---

## 超时配置

代理请求的超时由健康检查的 `timeout` 参数控制。对于正常的代理请求，Gatel 使用 hyper-util 客户端的默认超时设置。

---

## 完整配置参考

以下是 `proxy` 指令内所有可用节点的汇总：

```kdl
proxy {
    // 上游后端（至少一个）
    upstream "host:port" weight=N

    // 负载均衡策略
    lb "strategy" header="..." cookie="..."

    // 重试次数
    retries N

    // 主动健康检查
    health-check path="/path" interval="Ns" timeout="Ns" threshold=N

    // 被动健康检查
    passive-health fail-duration="Ns" max-fails=N unhealthy-status="status1,status2"

    // 请求头操作（发往上游）
    header-up "Name" "Value"
    header-up "-Name"  // 删除

    // 响应头操作（返回客户端）
    header-down "Name" "Value"
    header-down "-Name"  // 删除

    // DNS 动态解析
    dns-upstream interval="Ns"
}
```
