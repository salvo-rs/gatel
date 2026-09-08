# 运行时指标

## 健康状态

现有 `metrics` 路由新增两项 gauge，不自动创建监控端口或路由：

| 指标 | 含义 |
|---|---|
| `gatel_ready` | readiness 可服务时为 1（包括 degraded），本地阻塞故障时为 0 |
| `gatel_health_reason{reason}` | 指定原因当前存在为 1，否则为 0；同类多个资源故障只汇总为一个值 |

`reason` 固定为 starting、no_listeners、draining、persistence_unknown、reload_failing、
upstream_down、certificate_pending，共 7 条序列，始终输出零值。不把配置内容、错误详情、
站点/上游名称作为这些指标的标签。两项取自同一次健康报告；不承诺与其他指标形成原子快照。
普通配置拒绝可能是 degraded 且 `gatel_ready=1`；持久化结果不明为
`gatel_health_reason{reason="persistence_unknown"}=1` 且 `gatel_ready=0`。
该故障锁存于当前宿主生命周期，不能用后续内存提交清除；指标不是写入回执或自动重启授权。

## 配置代号与通知顺序

`gatel_generation` 是同一进程/事务存储中已上报成功代号的最大值，类型仍为 gauge。
Started、Reloaded、NoOp 的迟到通知不能把它写回旧值，内部使用原子 u64 最大值，
不经过有符号截断。新 App 创建独立 Telemetry；嵌入者不能跨新 store/boot 复用这项
累计最大值。通知尚未送达时仍可能落后于事务存储，不能作为写入前提或激活完成信号。
十进制导出保留整数值，不承诺下游浮点指标系统对任意 u64 都有精确表示。

`gatel_reloads_total` 仍按收到的各类结果分别计数，迟到结果不丢弃、不自动去重。
最近成功时间记录通知处理时刻，不是原始提交时刻；跨指标抓取不是一致性快照。

## 后台服务恢复

现有 metrics 路由输出以下固定指标，无需新监听器。service 仅为 probe、resolver、
census，不使用上游名、错误文本、panic 内容或请求数据作标签，含零值共 12 条序列。

| 指标 | 类型 | 含义 |
|---|---|---|
| `gatel_worker_starts_total{service}` | counter | 子任务开始执行工厂，包含首次启动，不保证启动成功 |
| `gatel_worker_exits_total{service,reason}` | counter | 将进入退避的意外退出，reason 为 panic 或 returned，不是已完成重启数 |
| `gatel_workers_backoff{service}` | gauge | 当前等待重启的受管服务数 |

主动关闭不算意外退出；退避被关闭/abort 时等待数回零，不增加启动数。普通下游
健康检查失败不等于 worker 退出。指标归属 Telemetry，跨配置代保留，进程重建后重置。
使用独立原子量，抓取不是跨字段的一致事务快照。panic=abort/进程崩溃无法保证计量。

## 阻塞工作预算

现有 `metrics` 终端现在追加其 BuildContext 注入的预算指标，不另开监听器，
也不自动添加监控路由。它们与认证、其他使用 Executor 的插件共用同一计量域，
不是“认证请求总数”。CLI App 的各路由/代共享预算；独立控制器彼此独立。

| 指标 | 类型 | 含义 |
|---|---|---|
| `gatel_blocking_jobs` | gauge | 已准入的排队与运行任务总数 |
| `gatel_blocking_running_jobs` | gauge | 已开始执行、尚未结束的闭包数 |
| `gatel_blocking_memory_bytes` | gauge | 预留的算法声明工作内存，不是 RSS |
| `gatel_blocking_jobs_limit` | gauge | 当前任务准入上限 |
| `gatel_blocking_memory_limit_bytes` | gauge | 当前声明内存准入上限 |
| `gatel_blocking_closed` | gauge | 关闭为 1，否则 0 |
| `gatel_blocking_admissions_total` | counter | 接受的工作预约 |
| `gatel_blocking_rejections_total{reason}` | counter | `closed`、`jobs`、`memory` 三种拒绝原因 |
| `gatel_blocking_completions_total{outcome}` | counter | `completed`、`panicked`、`abandoned` 三种结束结果 |

始终是 13 条预算序列，只有上述固定枚举标签，没有用户名、密码、哈希、路径、
算法参数或插件动态名称标签。其他原有 HTTP 指标仍有其配置来源的路由标签。
零值序列也输出，数值直接以无符号整数呈现。Prometheus 客户端如何存储和取整
超大整数不属于该文本输出的保证。

拒绝原因互斥，优先级为 closed → jobs → memory；两种容量同时不足时只增加
jobs，不重复计数。内存相加溢出也属于 memory 拒绝。缩容后在途数量或内存可以
暂时高于新上限，直到已有工作释放，这不表示计数损坏。

completed 表示闭包正常返回，包括密码验证结果为 false，不表示认证成功。
panicked 表示已开始的闭包展开 panic；abandoned 表示尚未开始的预约被丢弃。
取消 HTTP 等待者不会结束已排队/运行的工作，其完成计数仍由工作守卫更新。
panic=abort 或进程被终止时不保证能记录结束。这里不是工作超时、CPU 时间或
每租户公平性指标，也不暴露 panic 内容；默认 panic hook 的日志行为另行约束。

所有预算值从同一把锁下复制，一次预算采样内部一致；整个 HTTP/上游/预算
抓取不是跨所有模块的全局原子快照。计数使用饱和加法，达到 u64 上限后保持
上限，不回绕。成功调限、失败/stale/NoOp 候选、关闭与重载均不清零计数；
创建全新的预算对象才开始新的计量生命周期。

纯 `gatel_observe::prometheus(telemetry)` 不知道宿主执行器；嵌入式监控端点应
显式用 `append_blocking` 追加 `executor.metrics()`。标准 metrics 终端已接入。
抓取本身不消耗阻塞任务额度。若把监控路由放在同一 Basic Auth 后，预算耗尽
仍可能令抓取返回 503；系统不会绕过该认证。运维可显式选择受网络限制的
独立监控路由，避免缓存指标响应，不应无意将监控公开到互联网。

验证覆盖固定标签/无符号输出、拒绝分类、正常返回/panic、取消等待者后的计数、
饱和不回绕，以及真实 HTTP 重载后计数保留、关闭后可抓取和凭据不进入输出。
