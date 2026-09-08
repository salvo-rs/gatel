# 插件开发与组合契约

当前实现采用源码链接的 Rust 插件，不承诺 Rust 动态库 ABI，也不将配置脚本当作隔离沙箱。插件与宿主使用同一版本的运行时桥接接口。

## 请求体变换

请求体变换插件声明 `TransformRequestBody`，并读取 SDK `RequestBodyLimit` 约束输出，
避免替换 body 后绕过宿主/guard 限额。`decode` 是实际使用该契约的可选包，
包含随 Bytes 生命周期持有的准入许可和兼容状态复用，见 [DECODE.md](DECODE.md)。

## 秘密依赖与自动刷新

插件准备阶段应通过 `BuildContext::secret` 读取秘密。宿主会记录实际读取的引用与
私有比较摘要，随运行代保存；CLI reload 轮询发现变化/缺失/权限错误后走资源刷新
事务，无需插件自行监听文件。basic-auth 的文件密码哈希已使用此入口，换文件即可
更新后续请求的认证配置，失败保留旧代。

每代最多 4096 个不同秘密引用，重复读取去重；同一次准备重复读取同一引用却得到
不同值会失败，避免一代内悄悄混用。秘密内容及摘要不进入计划哈希、Debug 或指标。
`BuildContext` 新增共享 `secrets` 字段，嵌入式宿主构造上下文时须提供同代共享实例；
这是版本耦合 SDK 的接口变化。自行读取文件的插件、动态请求期读取、跨文件原子
轮换和紧急撤销不在本契约内；该接口不是对受信任 Rust 插件的文件系统沙箱。

全部插件准备完成后宿主封存依赖集合。封存之后 `secret`/依赖 `read` 会在 I/O 前
拒绝，插件应保留已经解析的运行时对象，而不是把准备上下文当作请求期秘密加载器。
只读变化检查不受影响。文件读取对实际打开的句柄检查类型、权限和大小，并最多
读取 64 KiB + 1 字节判断超限；环境变量秘密同样限制为 64 KiB。这不提供符号链接
路径沙箱、跨文件原子快照或对恶意文件系统的读取时间上限。

## 准备阶段与状态发布

HTTP 宿主在独立的候选注册表中解析声明状态，仅成功提交后发布注册条目。
失败或被更新提交超越的候选不替换活动注册表；兼容状态仍共享同一个 Arc。
这不是状态对象的深拷贝：插件 build/prepare 不得修改复用状态、启动脱管任务
或执行不可回滚的外部写入。共享连接池策略由宿主延迟到成功发布时更新。
注册表发布依赖宿主的 generation/source stale 检查，不是独立的 CAS 服务。

## 分层结构

CGI 协议工厂调用与交换 future 的 unwind panic 会转成通用交换失败；响应头发送前按失败路径处理，发送后终止流，不把 panic 内容写入响应。默认 panic hook 仍可能记录内容；panic=abort、进程退出、双重 panic 和插件自行启动的脱管任务不受此边界保护，不能将其称为沙箱。

依赖门禁使用离线 Cargo metadata 的声明信息，检查所有平台／optional／构建依赖并按真实 package 名解析别名，dev-dependencies 豁免。新增插件须加入 workspace 与 `xtask/src/layers.rs`；许可图也必须无重复条目和内部循环。检查器需要本地 Cargo 可用，解析失败会明确失败而不是跳过检查。

CGI Sink 正文输出以最多 8 KiB 分块，单消息通道先 reserve 再复制，避免插件提交大切片时在队列外另分配整个正文。此限制只覆盖适配器队列中的拷贝，不包括插件自行持有的输入切片、HTTP 层缓冲或进程总 RSS；输出回调仍须完整 await，取消即使只发送了前缀也不能恢复为成功。

CGI 输出生命周期由共享 Sink 强制检查：一次 head、零或多次 body、一次 finish。协议 future 返回 Ok 还必须具有 finish 标记；错误顺序、重复 finish、输出回调中途取消会永久标记失败。该检查不代替协议层的响应头、正文长度或 EOF/END_REQUEST 校验，也不能撤回已经发送的 HTTP 头；发送后的错误仍需终止响应流。

CGI 插件另有专用 `gatel-cgi-http` 适配层：共享固定脚本环境映射、可选有界输入缓冲、容量、连接期限及随 HTTP 响应持有的交换 future。`backend_directive(BackendFactory)` 提供后端无关入口：纯语法 validate → 不启动请求工作的 prepare → 由 Backend 接收不含 socket 的 Invocation。`directive(Protocol)` 保留网络便捷入口，内部网络 Backend 在受控 future 内连接后再交给 FastCGI/SCGI 的 ExchangeRequest。两种入口均通过 Sink 提交经过校验的输出，启动和交换共用绝对 deadline、panic 隔离与响应流所有权。该层不依赖 plan 或 gatel-http，不是所有协议的通用基类；子进程 CGI 仍须实现平台级进程树终止与回收，接口并未自动实现这些能力。插件必须惰性返回 future，不得在工厂内启动脱离作用域的任务。

| 层 | 职责 | 不应承担 |
|---|---|---|
| `gatel-plugin-api` | 纯元数据、作用域、阶段、失败策略、组合约束和指纹 | 网络、任务、Salvo Handler、编译器、共享状态 |
| `gatel-model` / `gatel-macros` | 配置节点、来源诊断、类型化解码 | 运行期副作用 |
| `gatel-plugin-runtime` | 当前版本运行时 SDK：Directive、DirectiveConfig、BuildContext、Bundle、单后端句柄、后台任务工厂 | 编译器、监听器、网络传输实现 |
| `gatel-plan` | Registry、规范化与编译；重导出 SDK 便于核心调用 | 插件业务实现 |
| `plugins/<name>` | 独立实现、测试、配置样例；按需声明运行时依赖 | 宿主 CLI、其他插件的内部实现 |
| `gatel/src/plugins.rs` | 唯一宿主装配入口 | 按 validate/run/capabilities 分叉的注册逻辑 |

元数据与运行时 trait 已从 plan 抽离，独立维护模式包仅依赖 SDK，不依赖编译器。SDK 仍按宿主版本链接，不承诺稳定 ABI；`BuildContext.state: Any`、Secret/文件访问和进程级 telemetry 尚未完成能力收紧，不能称为完整隔离接口。

## 接入步骤

### 阻塞执行与宿主预算

Executor 的固定基数指标已接入标准 metrics 终端，见 [METRICS.md](METRICS.md)。
计数来自工作生命周期，不依赖请求等待者继续存活，插件无需重复计数。

CPU 密集或同步阻塞插件应捕获 `BuildContext.blocking`，通过
`Executor::run(memory_bytes, move || ...)` 执行。它在提交 Tokio 阻塞任务前
原子预留一个任务槽与声明工作内存，无准入队列；拒绝时闭包不执行。
取消等待者不归还仍在排队/运行的额度，闭包真正结束（含 unwind panic）才回收。
必须在 Tokio runtime 内调用，不可将异步 future 当作阻塞闭包的有效工作。

默认额度为 16 项任务 / 256 MiB 声明内存。CLI App 持有自己的可配置预算，
跨其站点与 generation 共享；低层 Server::new 保留固定进程级默认池。
多个显式控制器不是同一个总预算。嵌入式宿主持有 `blocking::Budget`，用 `executor()` 生成无调限权限的
句柄，通过 `Server::with_blocking_executor` 及 `Resources.blocking` 注入；
各代必须复用同一个句柄。Budget 的 reconfigure 保留真实使用量，关闭后不能
靠调限重新开放；丢弃控制器不会自动关闭仍被执行器持有的预算。

`snapshot()` 提供一致的限额/占用/关闭状态。这是受信任插件的工作声明协议，
不是内存分配器、线程隔离或进程 RSS 限制。不得在候选 prepare 中调限或启动
工作，不得创建替代池绕过宿主。KDL `global { blocking jobs=8 memory="128MiB" }`
已接入：宿主还须提供匹配的 Resources.blocking_budget，prepare 只生成惰性
PreparedChange，GenerationStore 在版本复核后通过 PublishRuntime/PublishGuard
发布它；这是宿主协议，不是允许插件注册任意提交回调。没有控制器的宿主会
拒绝显式预算配置。删除配置恢复默认值，NoOp 不调限。当前 API 不提供 Admin API、自动
关闭时限、每租户公平性或 CPU 时间计量；宿主 shutdown 不会关闭可能被其他
Server 共用的默认池。密码认证是此 API 的首个生产调用方。

### 请求侧效果声明

宿主在执行链开始前以 `ExecutionIdentity::DEPOT_KEY` 写入类型化
`ExecutionIdentity`。它是选中路由、匹配器、执行链、全局配置及声明的上游配置
的稳定行为指纹，不是 generation 序号，也不是授权能力。缓存用其隔离热更新前后
的不同行为；未注入该上下文的嵌入式宿主会跳过缓存，cache-only 请求返回 504。
插件必须完整声明 `upstreams()` 并正确实现配置 hash；外部文件内容或后端实际数据
变化不自动改变此指纹，仍需 TTL/失效或相应资源版本机制。

`CacheLookup` 表示可能用缓存响应短路后续链。所有 Guard 阶段指令必须先于
该效果执行，跨 site/route 同样生效；反向 before 导致编译失败。缓存命中仍
扣请求额度。限流在下游返回后写当前额度响应头，避免缓存存入或重放旧额度。
外部缓存插件也应声明此效果，不能只依赖 Phase::Request 的同作用域排序。

通过 `DirectiveMeta::with_effects(&[Effect::...])` 声明请求侧行为：
`RequestBodyLimit` 安装流式限制，`ReadRequestBody` 读取或缓冲正文，
`CallUpstream` 调用上游，`ConsumeQuota` 扣减额度。编译器自动让 body limit
先于后三类效果，同样适用于不同名称的外部插件与跨 site/route 作用域；
显式 order 或 before 与强制约束冲突时拒绝编译，不静默降级。标准
body-limit、forward-auth、rate-limit 已使用该契约，效果集合参与构建指纹，
声明顺序与重复项不改变身份。

这里保证的是限制器安装及已知 Content-Length 的提前拒绝，不保证未知长度
正文溢出在扣额度或发起上游请求之前被发现。效果是受信任插件的声明，不能
检测未声明的实际 I/O，也不替代运行时资源授权。当前规则只覆盖 middleware
请求侧组合；terminal 固定在链末尾，规范化与计划生成共用 `order_route`
校验 middleware/terminal 双向 conflicts（包括内核 proxy），并拒绝 terminal
的 before 或效果要求其先于已存在 middleware 的组合。指向缺席插件的约束
不报错，middleware 声明先于 terminal 合法。响应压缩、缓存、身份、Upgrade/trailers
的完整效果模型尚待实现，不应把这些列入当前编译安全保证。

1. 在 `plugins/` 创建独立 crate，加入 workspace；依赖同版本 `gatel-plugin-runtime` 与 `gatel-plugin-api`，不依赖 `gatel-plan`。
2. 配置类型实现 `DirectiveConfig`；使用 `FromNode` 派生完成结构解码，`decode` 补充范围等语义检查。配置解析和 hash 不执行 I/O。
3. 实现 `Directive`，返回静态 metadata；用 `Bundle::new(name, version).with(...)` 导出包。
4. 宿主加入 optional dependency、显式 feature，在唯一装配入口注册；默认不启用可选包。
5. 在 xtask 的依赖白名单声明层级依赖。插件目录也受文件规模和事件契约检查，不作为架构豁免区。
6. 测试 feature 关闭时 capabilities 不含指令、配置明确拒绝；开启时验证配置、真实请求和 reload。

可执行样例是 `plugins/maintenance`：

```console
cargo run -p gatel --features maintenance -- capabilities
cargo run -p gatel --features maintenance -- validate plugins/maintenance/examples/maintenance.kdl
cargo run -p gatel --features maintenance -- run plugins/maintenance/examples/maintenance.kdl
cargo test -p gatel --all-features --test plugins
cargo test -p gatel --no-default-features --test plugins
```

`maintenance` 是 Guard 中间件；启用时返回 503、Retry-After、no-store，HEAD 不发送正文；关闭时继续执行后续链。它不创建后台任务，因而不是有状态插件生命周期的验收替代品。

## 组合规则

`DirectiveMeta::before(&[...])` 声明当前中间件必须先于命名中间件执行，`conflicts_with(&[...])` 拒绝同一执行链中的互斥中间件。名字没有出现在链中时忽略，不等同于必需依赖。

- 对站点 fallback 链、每条合并后的 site + route 链分别验证。
- 没有约束时保持原来的 scope / phase / order / name 稳定排序。
- 同作用域的阶段边界强制保留；显式约束可跨作用域，优先于用户 `order`。例如路由 body-limit 可以先于站点 forward-auth。
- 冲突或环在 normalize 阶段带来源诊断拒绝，不能进入 prepare；不是运行期临时调整。
- 约束列表排序去重后参与构建指纹；约束改变会触发重新编译。
- 当前约束仅适用于中间件，terminal 仍独立选择；尚不是完整的请求体/响应体效果系统，也不推断未知插件是否消费 body。

## 后续大框架的边界

### 已落地的资源与任务约束

`DirectiveConfig::prepare(cx)` 是统一初始化入口，返回 `PreparedDirective { handler, tasks }`。同一个 owner 在每个候选 generation 中只调用一次，handler 在继承路由、fallback 和多个 listener 间共享；task 工厂与 handler 可以直接共享同一份 generation-local 资源。旧 build/background hooks 由默认 prepare 串接，已有插件无需立即改写；只实现 prepare 的插件无需再实现 build。所有工厂仍须惰性，返回的候选被拒绝时，handler 与未运行工厂一起销毁。

owner 而非配置哈希是实例身份：两个路由即使使用相同 terminal 配置，也不能共享状态/任务身份。同一 owner 的重复出现必须具有相同配置指纹，否则 prepare 拒绝。terminal 现在与中间件一样绑定其 route owner；此前 terminal 未绑定路由，会导致跨路由状态别名及后台任务误去重。本修复改变 terminal 状态身份，不保留该错误共享行为。需要站点级共享预算时应显式声明站点级中间件，不能依赖 terminal 的偶然共用。

宿主对每个插件实例按 `DirectiveConfig::upstreams()` 构造 allowlist。`BuildContext::upstream()` 返回 `UpstreamHandle`，其 `ask()` 不再接受资源 ID；直接使用 context 内的 upstream 集合也会在实际请求处校验权限，不能通过忽略 `has()` 访问未声明后端。白名单再包装不能扩大原始授权。forward-auth 已迁移至单资源句柄。

默认 prepare 调用的 `DirectiveConfig::background(cx)`，或直接放进 PreparedDirective 的任务，都只描述 BackgroundTask 意图，不能启动工作。宿主按 owner 去重，每个 generation 最多 64 项任务；空名字与同一实例内重复名字拒绝 prepare。工厂在已提交 generation 激活时运行；现有 accept loop 看到新 commit 时也确保先激活，再服务首个请求。重复激活和 NoOp 不重复启动。

任务属于 generation，而非监听器。生产 prepare 使用 `PluginTasks::managed`，向 Server 所有的 Retirements 预留一个作用域名额。旧连接持有旧 generation 时任务保留；最后一个持有者释放时把任务作用域交给宿主异步关闭，不继续持有旧 Runtime、路由或 Committed。失败或丢弃的候选不启动任务/清理 job，立即归还预留名额。独立 `PluginTasks::new` 无宿主托管，调用方仍须显式 await shutdown，否则 drop 只能 abort。

宿主最多跟踪 64 个包含后台任务的候选/活动/连接固定/清理中 generation，每代最多 64 项任务；无后台任务的配置不占名额。清理 job 完成且任务作用域确实停止后才释放容量。超限在 prepare 返回错误并保持 LKG；释放旧 generation 后可重试。该限制目前是固定上限，尚未暴露 KDL 配置或监控指标。

Server shutdown 在连接 drain 后封闭新的插件作用域注册，并并发关闭所有已登记作用域，包括仍由外部 Arc 持有的旧/当前 generation，再 join 清理 jobs。关闭调用取消后仍保留登记与 join handle，后续可重试。强行销毁宿主或停止 Tokio runtime 是紧急 abort 路径，不保证异步清理；正常退出须调用 Server/App shutdown。

需要异步清理的插件使用 `BackgroundTask::cooperative(name, |mut shutdown| async move { ... })`，返回 `Result<(), String>`。`shutdown.requested().await` 为持续有效的关闭信号，先发信号再等待也不会丢失通知，克隆接收端可以共同观察。`PluginTasks::shutdown().await` 先通知全部任务，普通 new/fallible 任务立即 abort；协作任务共享 5 秒宽限期，不是每项各 5 秒。宽限结束后 abort 剩余任务并 join；正常完成清理记 Completed，强制取消记 Cancelled。5 秒只约束合作等待，不承诺可以中断同步阻塞或让任意 OS 清理必然完成。

并发 shutdown 调用会等待同一次清理；调用方取消 shutdown future 时，会 abort 尚未结束的任务并保留 join handle，下次 shutdown 可继续 join，不会因 JoinHandle 被直接丢弃而脱管。ShutdownSignal 在 SDK 中只负责通知，不自行启动工作。直接构造 BackgroundTask 的 Rust 调用方需适配新增 cooperative 标记和接收 Shutdown 的 run 工厂；new/fallible 构造器调用方式不变。

这是带协作关闭与宿主退役管理的异步任务生命周期，不是完整进程 supervisor：不提供自动重启、panic/readiness 联动、跨 generation worker 复用或 OS 进程树终止。任务必须正确响应 future drop，不可自行派生未受管任务；同步阻塞、插件主动持有宿主 generation 形成引用环仍需避免。源码链接插件仍是受信任代码，不是安全沙箱。

证据：`gatel-plugin-runtime` allowlist/绑定句柄测试，`gatel-http/src/plugin_tasks.rs` 取消与幂等测试，`gatel-http/tests/plugin_lifecycle.rs` 真实事务、跨 listener 去重、LKG、NoOp 与 generation pinning。

可失败的工作使用 `BackgroundTask::fallible`，返回 `Result<(), String>`。宿主 `PluginTasks::outcomes()` 按声明顺序报告 Pending/Running/Completed/Failed/Panicked/Cancelled；工厂与 future 的 panic 均被捕获。快照不保留错误内容，避免管理接口泄露秘密；Rust 默认 panic hook 仍然可能输出 panic 文本，因此插件自身也必须遵守秘密脱敏约束。此状态接口尚未接入 readiness 或自动重启。

### 剩余顺序

按以下顺序扩展，而不是向 `BuildContext` 追加无限服务：

1. **资源服务**：继续补类型化 Secret、Resolver、Dialer、容量预算；在已有单后端授权之上收紧剩余服务，不直接下转型全局 State。
2. **生命周期**：在已有协作关闭与有界退役管理上补进程级 kill/reap、失败观测、有状态 worker 复用和状态兼容性校验；尤其不能将 abort 当成 CGI 进程树回收。
3. **非 HTTP terminal**：请求环境、受限双向流、取消、stderr 和退出结果采用协议中立接口；FastCGI/SCGI/子进程各自实现 transport，不硬套 HTTP 连接池。
4. **效果校验**：在显式顺序约束之上增加 body 读取/重放、缓存、认证、编码等能力声明；只有真实组合测试证明有效后再推广抽象。
5. **SDK 验收**：runtime bridge 已移出 plan；继续由至少一个有状态协议插件验证接口，并补第三方插件在仓库外独立构建的夹具。

上述剩余项不是已实现能力。CGI 的进程树回收、协议流控和资源隔离仍需单独完成。
