# 稳定性验收

## 流量与热重载短测

```sh
GATEL_SOAK_SECONDS=60 cargo test -p gatel-http --test soak --locked -- --ignored --nocapture
```

该测试默认忽略，显式运行时使用 4 个 Tokio 工作线程、一个进程内 gateway 和真实
本地 TCP 后端。每轮顺序发送首页、唯一 API 路径、200 KiB 静态文件、metrics、
1001 字节 Range 请求，再切换三份配置之一。它不是并发客户端吞吐基准。

未设置 `GATEL_SOAK_SECONDS` 时使用 30 秒；设置后必须为可表示为 u64 的正十进制整数。
空串、0、带符号、单位、空格、非 Unicode 或溢出输入在创建夹具/监听器之前失败，
不再静默退回短测。开始时输出实际请求时长；循环比较 elapsed，避免绝对 deadline 加法溢出。

每次配置提交必须成功，并协调真实 Supervisor（探测及 census）、清理不再使用的
状态和指标。三份配置覆盖 pool 容量变化、endpoint 身份变化和探测启停；Range 的
206、Content-Range 和正文必须正确。结束时监听器与 Supervisor 必须在 5 秒内排空。

80 轮预热后取得基线，每 20 轮和结束时检查 state slots、指标行数、日志 sink、
pool idle、Tokio 活动任务数、cache 资源元数据数。每项允许基线外 8 个计数的既有
波动范围；运行不足 101 轮不能通过。此门槛不是零泄漏证明。

Linux RSS 从 `/proc/self/status` 的 VmRSS 读取，不假定页大小，仅记录不做断言。
进程还包含测试后端和客户端，RSS/任务统计不是独立生产 gateway 的精确归因。

## 尚待发布级验证

2026-09-09 显式 60 秒短测：Windows 6,134 轮、Linux 8,547 轮通过；基线/末次样本中的
state=4、metrics=199、sinks=1、cached_resources=6，idle/tasks 在已有允许范围内变化。
Linux 进程 RSS 为 18,408→26,540 KiB，仅记录，不归因或断言零增长。Windows 短测在本批
时长参数严格校验修改前执行，随后解析回归和编译检查通过；Linux 的完整 soak 目标在修改后
使用 --include-ignored 执行，包含三项测试。两平台顺序运行，不作跨平台性能比较。
本批另显式复验 PHP/FPM 互操作，细节见 CGI.md；并不因此完成多小时、多协议发布级长测。

第二百一十五批为命名来源 watcher 用例增加测试专用阶段记录：setup、各次同步 attempt/
文件写入、activate、HTTP 请求、watch/import 更新等待、watcher join、App shutdown 和清理。
`scope_ended` 记录壁钟耗时与是否正在 unwind，不等同于该阶段后面的断言已经通过。
既有 HTTP 3 秒、watch/import 12 秒期限保持不变；activate/join/shutdown 新增 20 秒
协作式测试期限，并用虚拟时钟 pending future 验证超时错误包含阶段名。这不是生产超时，
不能中断同步文件 I/O、阻塞的执行器线程或 Tokio runtime 的最终析构等待。
复现时使用 `cargo test -p gatel --all-features --bin gatel named_file_sources_publish_reload_and_keep_lkg_on_failed_observation -- --nocapture`。
本次 Windows 阶段输出显示 watch 更新等待约 5.26 秒，与五秒 polling fallback 相符；
这不能解释此前 77.42 秒的慢运行，也不构成受控压力验收。

第二百一十四批已将 format 的所有 CLI 调用接入共享 `tests/support/cli_process.rs`：
120 秒子进程等待限额、终止并回收、文件捕获、PID/启动/完成耗时，以及输入类别/命令标签。
超过 1 MiB 的捕获输出明确使测试失败，不再静默截断；输出文件的磁盘增长不是硬限额。
后续复现可运行 `cargo test -p gatel --all-features --test format -- --nocapture`，
保留逐次输出。该改动取代下段记录中“普通 output 等待”的现状，但不解释历史慢运行根因，
也不意味着给生产 CLI、OS 不可中断 I/O 或后代进程树施加了强制 deadline。

第二百一十三批全量顺序回归全部通过（Windows 1360、Linux 1375，各另有四项忽略），
但 Linux 宿主 watcher 用例与 format 无效输入用例触发超过 60 秒提示；对应整组耗时
77.42、88.74 秒。同镜像 `-p gatel` 独立复测为 5.67、0.18 秒，但发生重编译且
运行组合不同，不能据此确定根因或关闭性能问题。format 当前仍使用普通子进程 output
等待，缺少每次子进程/输入类别的阶段证据；watcher 测试有请求与轮询期限，但同步阶段
和 shutdown 仍需更清晰的定位。下一步应补充这些证据并做受控压力复现，不放宽断言。

2026-09-08 全工作区检查发现两项间歇性失败：Windows 的 SCGI
`configured_idle_timeout_applies_before_no_body_response_is_published`，以及 Linux 的
`failed_account_candidate_does_not_replace_active_shutdown_ownership`（activate 一秒超时）。
首次运行两平台任务并行，随后单测及顺序全量重跑通过；没有据此确认资源竞争为根因，
也没有放宽断言。发布前应在受控 CPU/I/O 压力下记录各阶段耗时并定位，不能仅靠重试
成功将此问题关闭。该记录不表示上述测试已被忽略或从默认门禁移除。

第一百九十八批顺序全量回归通过（Windows 1324、Linux 1339 项，各另有 4 项忽略），
但 Linux precompress CLI 六项测试组耗时 62.89 秒，两项触发超过 60 秒提示；同镜像
独立重跑为 0.32 秒。当时 CLI helper 使用阻塞的子进程 output 等待，该结果没有提供
足够证据将耗时归因于压缩、文件 I/O、进程启动或调度。后续应补阶段耗时及受控负载
验收；不能把独立重跑通过解释为已经修复性能问题。

第一百九十九批为后续复现增加可选 precompress 阶段事件，并将对应集成测试改为
文件捕获输出、PID/总耗时记录和 120 秒子进程等待上限。新增真实 CLI 测试验证阶段
事件成对、耗时字段、默认关闭，以及事件不泄露资产名/正文；当批没有复现原 62.89 秒
延迟或独立注入子进程超时，因此不能据当批结果宣称已查明原因或验收超时故障路径。

第二百批补充了独立超时注入：复用测试二进制作为输出开始等待标记后永久等待的
直接子进程，五秒截止后要求捕获到标记、stdout/stderr、非成功退出状态，Linux
还验证 SIGKILL；只有 wait 回收完成后才返回结构化超时结果。该检查不依赖外部
解释器、端口或额外 ignored 用例，不证明后代进程树回收、OS 不可中断等待，
也不解释原预压缩慢运行原因。

另有默认执行的 `retired_generations_and_caches_are_released_under_identity_churn`
回归：120 轮使用全新的 upstream、endpoint、site、route 和日志路径，每轮两次
真实请求，协调 Supervisor 并清理状态。除了计数检查，还持有弱引用，要求前 119
个 generation 和 cache store 的强引用全部消失；当前一代则必须仍可服务。
该检查曾发现发布暂存记录延迟释放前一代 cache，现已通过独立发布后清理修复。
Windows/Linux 均已实际通过；它不替代小时级身份变化压力测试。

- 数小时运行、更多配置与资源身份的持续变化，不能只在三份配置间切换。
- 并发客户端、慢客户端、连接中断、资源压力、TLS/ACME、H2/H3、WebSocket、CGI、
  编码与解压插件的独立负载和组合。
- 独立进程的 FD、堆内存、任务、队列、尾延迟及 CPU 数据，以及 Windows/macOS。
- 对历史暂态失败保留证据；重跑通过不等于原因得到解释。

实际运行结果按批次记录在 [ROADMAP.md](ROADMAP.md)，不能从测试代码存在推导验收完成。
