# Runtime 控制面

本文档说明 Gatel 的 runtime service 模型、用于修改该模型的 admin API、运行时路由与 drain 语义，以及通用代理原语和外部部署控制器之间的边界。

## 范围

Gatel 的 runtime 层刻意保持克制：

- 它负责通用的数据面原语，例如 service、route、target group、target、TLS 绑定、健康门控激活、drain 和可观测性。
- 它不负责 deploy 工作流、版本选择、rollback 策略、容器命名、角色语义或控制器 UX。

像 `kamalx` 这样的外部控制器，应当把部署意图翻译成对这些 runtime 原语的 API 调用。

## Runtime Service 模型

Runtime 状态与静态 KDL 配置分离存储，并在提交时以原子方式合并进 live router。

层级结构：

1. `service`
2. `listener`
3. `route`
4. `target_group`
5. `target`

稳定标识：

- `service.id`
- `listener.id`
- `route.id`
- `target_group.id`
- `target.id`

`service` 的职责：

- 声明 runtime host 和可选的 runtime TLS 策略。
- 拥有 listeners 和 routes。
- 提供 optimistic concurrency 所依赖的 revision 边界。

`route` 的职责：

- 定义 host/path 作用域。
- 定义额外的请求匹配器。
- 选择负载均衡策略。
- 拥有一个或多个带权重的 target group。

`target_group` 的职责：

- 提供 group 级别的权重，用于流量切分。

`target` 的职责：

- 保存地址、权重、激活状态和 drain 超时。

Target 状态：

- `warming`：已注册，但在健康检查成功前不会进入负载均衡集合。
- `active`：可以接收流量。
- `draining`：不会再接收新流量，但已有 in-flight 工作会在 drain 截止前继续完成。
- `failed`：因激活或健康检查失败而被隔离。

持久化与恢复：

- Runtime 状态写入主配置文件旁边的 `*.runtime.json`。
- 写入是 crash-safe 的：先写临时文件，再原子 rename。
- 进程重启时，Gatel 会恢复 runtime snapshot、先做校验，再重建 live router。
- 损坏的 runtime 状态不会被悄悄应用，而是通过 admin API 和 metrics 暴露出来。

## Admin API

Admin API 是资源式 HTTP API。

认证方式：

- `admin_auth_token`：完整读写权限。
- `admin_read_token`：只读权限。
- `admin_write_token`：写权限，同时也允许读。

并发控制：

- 变更请求可以携带 `If-Match: "<revision>"`。
- revision 过期时会被拒绝，不会做部分写入。

主要读接口：

- `GET /health`
- `GET /config`
- `GET /runtime/state`
- `GET /services`
- `GET /services/{service}`
- `GET /services/{service}/routes`
- `GET /services/{service}/routes/{route}`
- `GET /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `GET /upstreams`
- `GET /metrics`

主要写接口：

- `PUT /services/{service}`
- `PATCH /services/{service}`
- `DELETE /services/{service}`
- `PUT /services/{service}/listeners/{listener}`
- `PUT /services/{service}/routes/{route}`
- `PATCH /services/{service}/routes/{route}`
- `DELETE /services/{service}/routes/{route}`
- `PUT /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `PATCH /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`
- `DELETE /services/{service}/routes/{route}/target-groups/{group}/targets/{target}`

变更保证：

- 提交前先做校验。
- runtime + static 的合并配置作为一个整体重建。
- TLS reload 和 router swap 在同一个 apply 步骤中完成。
- 如果 apply 失败，持久化的 runtime 文件会回滚到上一个 snapshot。

## 流量切分语义

Runtime route 的排序是确定性的。更具体的 path 作用域优先于更宽泛的 path。

在相同 host/path 作用域内：

- 先看协议作用域（`http` vs `https`）
- 再用 header/cookie/query/method 等 matcher 细化候选集
- 语义上冲突的兄弟 route 会在写入时直接被拒绝

支持的切分原语：

- 带权重的 target group
- 基于 header 的 canary
- 基于 cookie 的 canary
- path 作用域流量策略
- host 作用域流量策略
- 基于 cookie、header 或 hash 输入的 sticky routing

运行时可选的负载均衡策略：

- `round_robin`
- `random`
- `weighted_round_robin`
- `ip_hash`
- `least_conn`
- `uri_hash`
- `header_hash`
- `cookie_hash`
- `first`
- `two_random_choices`

可观测性：

- `gatel_route_matches_total`
- `gatel_backend_selections_total`
- runtime target state metrics
- debug tracing，可解释路由为什么命中、上游为什么被选中

## Drain 行为

当 target 进入 `draining`：

1. 它会立刻从 active 负载均衡集合中移除
2. 新请求不再选中它
3. 已建立的响应流和 WebSocket 隧道继续持有 runtime activity guard
4. 该 target 会在以下任一条件满足时从 runtime 状态中删除：
   - active runtime activity 归零
   - `drain_timeout` 到期

运行模型：

- 超时按 runtime target 单独配置。
- 短请求通常会很快归零，因此目标会被立即清理。
- 长连接 HTTP 响应流和 WebSocket 隧道的活动度，独立于 router 成员关系追踪。
- 即使 target 已经从路由状态中移除，已经建立好的 stream 或 tunnel 仍可自行收尾。

这种行为给控制器提供了可预测的切流时点，同时避免过早打断 in-flight 工作。

## Runtime TLS 与 Host 策略

Runtime TLS 状态可以更新：

- 手动证书 / 私钥引用
- HTTPS redirect 策略
- canonical host redirect 策略

校验规则：

- runtime/runtime SNI 冲突会被拒绝
- static/runtime host 冲突会被拒绝
- canonical host 必须显式声明在该 runtime service 上

Reload 模型：

- Runtime TLS 变更会先合并进 live config snapshot
- `TlsManager` 会基于合并后的配置热重载，无需整进程重启
- 新连接会立即看到新的证书与 redirect 策略

运行限制：

- Runtime listener 是逻辑上的路由 listener，不是操作系统层面的 bind/unbind
- Runtime TLS 要求 HTTPS listener 和 TLS manager 在进程启动时就已启用
- 已经开始的 TLS 握手和已经建立的代理流，不会被追溯性重写

## 通用代理原语边界

属于 Gatel 的内容：

- runtime 路由图
- target 激活与 drain
- 健康检查
- 流量切分与 affinity
- runtime TLS 与 host redirect
- 持久化、恢复、metrics 和审计日志

属于外部控制器的内容：

- deploy / rollback 命令
- release 命名与选择
- app / accessory / role 映射
- 容器编排
- rollout 策略和面向用户的工作流

控制器应当把 Gatel 当作可复用的代理控制面，而不是自带发布语义的部署产品。
