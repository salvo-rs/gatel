# 可选管理接口（默认内存模式，可选检查点）

检查点模式使用独立 `SourceWriteError::PersistenceUnknown`：宿主写盘结果不明时，
必须区别于 Conflict、Observation 和 Unavailable。HTTP 映射为 503，静态正文明确告知配置可能
已经应用、持久化结果未知，应先检查；不附 ETag 或 Retry-After，不授权自动盲重试。
默认内存写入实现和计划层 SourceWriter 不产生该错误；显式检查点模式已接通该错误路径。
客户端仅查询当前 Version 不能证明磁盘结果或解决回执丢失，仍需后续幂等/恢复协议。

构建 feature：`admin-api`，默认关闭。目前实现了宿主内部的 HTTP 处理器和显式启动的
loopback HTTP/1 监听；**仅启用 feature 不会打开端口，必须显式提供下面两个 run 参数。**
远程 TLS、预写日志与幂等回执仍待实现。处理器带 `Connection: close`，
监听禁用 keep-alive；不能原样用于 HTTP/2。

```sh
gatel run main.kdl --admin-listen 127.0.0.1:9090 --admin-token-file admin.token
```

两参数必须成对提供，仅在带 `admin-api` feature 的构建中可用。token 文件路径相对工作目录，
采用现有 Secret 文件读取机制：普通文件、64 KiB 读取上限、末尾 CR/LF 去除、Unix 拒绝 group/other 权限；
读取后再检查 32～256 字节 token 规则。Windows 依赖操作者设置私有目录 ACL，不伪称已审计 DACL。
文件/父目录须受信任，现有读取机制不是恶意本地路径替换的安全沙箱；Secret 读取产生的全部临时副本也未承诺擦除。
token 在启动时读取，不随文件 watcher 自动轮换。默认模式的 `--dry-run` 检查管理绑定并排空退出；
附加 `--checkpoint-journal DIR --checkpoint-instance ID` 时会初始化/保存检查点，详见 [CLI](CLI.md)。

## 监听与关闭

- 宿主显式绑定 socket 后调用 `AdminServer::start`；它再次验证本地地址必须为 loopback，拒绝 wildcard 明文监听。不提供远程明文的例外开关。
- 最多 32 个连接任务；满额后直接关闭新 socket，不为发送错误另起任务。accept/join 循环同时回收已完成任务，handler 另有八个写入/其他请求槽和两个诊断读取槽。
- Hyper HTTP/1 限制 32 个 headers、16 KiB 解析 buffer、5 秒 header read timeout。这不是内核 socket buffer 或进程整体内存的硬上限；未启用 upgrade 或 HTTP/2。
- `shutdown()` 停止接受连接，取消并等待所有连接任务，包括未完成 headers 和已经认证的慢 body；丢弃 handle 会异步发出关闭信号，但不提供完成回执。
- 已交给 App 的配置任务独立拥有生命周期，关闭管理 socket 不撤销它们；CLI 先关闭 AdminServer，再 `App::shutdown()` 排空已接收事务。
- accept 失败会关闭该监听并取消其连接；CLI 监听其任务结束，触发全进程排空并返回退出码 3，而非静默继续业务服务。暂未提供自动重启或独立监听健康指标。
- `terminated()` 的等待可取消而不丢失监督句柄；完成结果只消费一次，调用者必须保留已收到的错误，后续 shutdown 不重放该错误。取消 shutdown 的等待仍会让已拥有的网络清理继续，但只有显式等待完成才有排空回执。

## 请求契约

可选 `--admin-read-token-file PATH` 提供独立的只读 Bearer token，沿用私有文件、
32～256 ASCII 字节及末尾换行处理规则。仍需原有 `--admin-listen`/`--admin-token-file` 配对；
这是额外的监控凭据，不是无写入 token 的独立监听模式。读写 token 必须不同，均在绑定监听器前校验。
只读 token 仅允许精确 GET `/v1/config/version` 与 GET `/v1/config/status`，其他路径/方法（含 query）
返回 403，且不读取 body、不消耗写入槽、不改变配置意图或失败状态。写入 token 保留原有权限。
两类 token 的请求仍受 Origin、读取容量和连接边界保护。token 只在启动时加载，修改文件不会热轮换；
资源级权限、动态撤销、多 token 管理和审计链路仍未实现。普通及 checkpoint-only 模式均支持只读 token。

- 宿主必须预先注册 admin 来源，并通过私有配置提供 32～256 个可见 ASCII 字节的随机 token；长度检查不证明随机性。不要把 token 放在命令行、URL 或日志中。
- 所有请求先检查唯一的 `Authorization: Bearer <token>`；当前要求该前缀的精确拼写。存储的是带格式域的摘要，固定长度摘要使用 `subtle` 常量时间比较；构造参数用 Zeroizing 释放，但不声称 HTTP header 副本也被擦除。
- 拒绝任何 Origin header，不提供浏览器跨域管理或 CORS 支持。认证在请求体读取和版本查询之前。
- `GET /v1/config/version` 返回当前 whole Version 的强 ETag，不返回配置内容。所有响应 `Cache-Control: no-store`。
- `PUT /v1/config` 替换 admin 来源的全部 KDL 内容；要求唯一且精确匹配当前 ETag 的 If-Match，以及精确 `Content-Type: application/kdl`。不接受弱 tag、列表、通配符、content encoding 或 URL query。空 body 表示该来源的空贡献/tombstone。
- 请求体流式累计不超过 1 MiB，累计读取时限 5 秒，最多 65,536 个 frame；拒绝 trailers、非法 UTF-8、声明长度不一致和 Content-Length/Transfer-Encoding 同时出现。节点/深度/禁止 import 的规则沿用绑定文本入口。frame 限额意味着高度碎片化的合法内容也可能被拒绝。
- 写入及其他非诊断请求最多八个同时占用处理器；精确匹配的 GET `/v1/config/version` 和 GET `/v1/config/status` 共用独立的两个只读槽，慢写入不能占用这两个槽。配置提交另有 App 的总任务限额。字节计数在复制每个 frame 前检查；底层 HTTP 库已分配的 frame、header 和调用方内存不由这里限额。5 秒仅覆盖 body 读取，不是 prepare/activation 的强制中断时间。

## 响应与事务

配置 PUT 的成功响应提供 `X-Gatel-Durability: memory|checkpoint`：前者仅确认内存应用，
后者确认本次写入的检查点保存完成（包含 NoOp 的检查点收尾）。checkpoint 成功正文为
`applied and checkpoint saved`，不再错误描述为仅应用于内存。
持久化结果不明的 503 提供 `X-Gatel-Durability: unknown`；其他错误与只读查询不提供此头。
该头不是请求 ID、WAL 回执、断电保证或可用于盲重试的幂等凭据；客户端仍需检查状态和版本。

### 结构化状态查询

只读预留仅隔离处理器层的慢写入：如果底层 32 个连接槽被慢 headers 或其他连接耗尽，
新诊断连接仍可能无法进入；CPU、内核资源及短暂状态读锁也没有独立隔离。该机制不是完整抗 DoS 保证。
真实 HTTP 回归持有八个未完成 body 的写请求，验证版本/状态读取仍成功、第九个写入被拒绝，
只读槽耗尽时查询亦返回 503；关闭后全部处理器槽归还，未完成 body 不改变配置意图。

`GET /v1/config/status` 使用相同 Bearer 认证、Origin 拒绝、请求容量限制和关闭边界，
不读取请求 body，不触发文件读取/重载/检查点保存。成功返回 `application/json`、`Cache-Control: no-store`。
仅接受 GET，不接受 query；其他方法返回 405 和 `Allow: GET`。

| 字段 | 含义 |
|---|---|
| schema_version | 当前为 1 |
| configuration_etag | 可用于配置 PUT 的完整 If-Match 值，与本次配置状态同次取样 |
| generation | 十进制字符串，避免 JavaScript 丢失 u64 精度 |
| last_activated_generation | 最近完成宿主激活的代号，十进制字符串；初始为 0，不是持久化确认 |
| published | 是否已有发布代；不证明宿主激活完成、监听器可用或保存成功 |
| observed_differs | 最新意图修订与发布构建输入不同，沿用下文版本接口的语义 |
| last_failure | null 或 attempts/stage；阶段限定 observation/normalize/prepare/other，无错误详情 |
| write_mode | 当前 Admin 写者为 memory 或 checkpoint，不是保存完成回执 |
| persistence_unknown | 当前宿主的持久化结果不明健康锁存状态；false 不证明磁盘已确认 |
| health | state 为 ready/degraded/not_ready；ready 布尔与固定、去重的 reasons 代码列表 |

状态查询 200 只表示查询成功，即使 health.ready=false 也返回 200。配置状态和健康状态
分别取样，不承诺跨字段的全局原子快照。健康可在配置版本不变时变化，因此不把配置 token
放在该响应的 HTTP ETag 中，也不返回 304；客户端从 configuration_etag 读取写入前置条件。
不输出完整来源、路径、资源名称、token、错误摘要或快照哈希。冷恢复后的 last_failure=null
不证明被保留的最新意图有效：检查点不保存原进程的失败诊断和次数。

激活代号先于配置状态读取，因此 `last_activated_generation` 不大于本次 `generation`，
但并发激活可能使取样的激活代号暂时落后。两者不同提示发布/激活进度不同，不能据此判断失败；
后续重载时旧配置仍可 ready，而新发布代尚未激活。两者相同也不是当前监听器可用或磁盘保存成功的证明。
查询不等待重载执行锁或激活锁，避免在最需要诊断的窗口阻塞管理读取。

### 版本查询与写入结果

`GET /v1/config/version` 同时返回 `X-Gatel-Observed-Differs: true|false`，
与 ETag 来自同一次事务状态取样。true 表示最新已接纳来源修订与当前已发布构建输入不同：
可能是语义拒绝、准备中的更新，也可能是有效 NoOp；**不是最新配置失败或有效计划不同的证明**。
尚无已发布配置时为 false。该字段不含来源内容，不表示磁盘保存或运行时激活确认。
冷恢复保留该差异，但检查点不保存历史失败诊断/次数，因此恢复后的 ready 不证明最新意图已验证；
不要通过两份修订不同就伪造 reload failure。后续有效 NoOp 也可能继续保留差异。

| 状态 | 含义 |
|---|---|
| 200 | 查询成功，或配置应用/NoOp；检查点模式的写入还完成了检查点保存，查询成功不证明写盘 |
| 401 / 403 | 未认证 / 不允许的 Origin |
| 404 / 405 | 未知路径 / 不支持的方法 |
| 428 / 412 | 缺少写入前置条件 / 版本不匹配或读取 body 期间发生并发更新 |
| 400 / 408 / 413 / 415 | body 错误 / 超时 / 超限 / 不支持的内容格式 |
| 422 | 配置拒绝或资源准备失败；客户端只收到固定摘要，不透出内部诊断 |
| 503 | 容量不足、关闭、来源不可用、事务 Stale 或持久化结果不明；不得无条件重试旧写入 |

请求拿到的版本在 body 读取之后仍会由 App 事务重新校验，不能用新版本替过期请求背书。
写入结果不附带一个事后重新采样的 ETag，因为它可能已经属于其他请求；需要继续修改时重新查询。
认证/body 校验失败不会提交配置观察；有效请求中的 KDL 解析失败会按正常观察失败影响配置健康，
语义拒绝则可能保留新的意图修订。客户端需要重新查询版本，而不是假设所有非 200 都未改变意图。

默认模式的写入 200 **不表示已持久化**。显式检查点模式的写入 200 表示完成检查点保存，
但尚无 WAL 或幂等请求收据。断连后可能已经应用并保存，因为 App 拥有已接收任务直至
激活/检查点收尾结束。CLI 关闭路径取消/排空尚未提交的慢 body，然后排空配置任务。
受限 admin 来源仍能改变自己的 runtime 资源，这不是针对恶意授权管理员的完整资源沙箱。

检查点模式通过统一会话协调启动、文件 watcher 与 Admin；保存失败会锁存写者，不自动修复。
保存或恢复确认结果不明还会锁存 `persistence_unknown` 健康原因，使 readiness 不就绪；
liveness 不失败，已有业务连接不因此直接中断。后续内存提交不能清除此状态，需显式协调恢复。
真实 HTTP 回归覆盖 `health "ready"` 从 200 转为 503（no-store，静态持久化故障原因）、
`health "live"` 保持 200，以及新生命周期完成检查点恢复后 readiness 回到 200。
底层意图日志尚未接入生产请求，后续写入顺序和崩溃窗口要求见 [持久化设计](design/admin-journal.md)。

## 当前验证范围

处理器测试使用实际 App 编译/准备/发布与 listener，直接调用管理处理器覆盖：
认证/前置条件/容量拒绝不轮询 body、流式超限/非法输入、虚拟时钟慢 body、
body 尚在读取时其他写入完成后的 CAS 冲突，以及解析失败与 NoOp 恢复。
另有真实 TCP HTTP/1 测试覆盖 401、ETag 查询、KDL PUT、chunked 超限、
32 个连接占满后的新连接关闭、超量/超大 headers、虚拟时钟慢 headers、
未完成 headers/body 的 shutdown、丢弃 handle 后连接关闭及 wildcard 拒绝。
跨进程 CLI 测试覆盖私有 token、ETag 查询/PUT、并存文件热更新和日志不包含 token；Linux 使用
SIGTERM 并验证成功退出，Windows 当前用进程 kill 收尾，不能据此声称 Windows 控制台信号优雅退出已验收。
尚未进行外部管理客户端、TLS 或长时间连接攻击验收。

另有监督边界回归：取消 `terminated()` 等待后真实 HTTP 仍可服务，正常结束后再 shutdown
不会二次轮询 JoinHandle；取消 shutdown 等待仍关闭真实连接并归还全部连接槽；注入任务
错误验证失败回执只消费一次。最后一项不是实际 OS accept 故障或真实 CLI 故障退出注入。
