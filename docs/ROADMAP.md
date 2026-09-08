# Gatel v2 后续完善计划

更新：2026-09-09。状态：按用户要求冻结本轮功能范围，本轮验证与交接已完成；以下未完成项保留为后续规划，不继续自动扩展。

## 本轮收尾与后续入口

本轮以现有功能基本稳定为结束条件，不以实现全部历史规划为条件。
保持现有内存和 checkpoint-only 生产路径；实验 WAL 编解码不开放生产入口，不承诺完整 WAL、幂等回执或在线轮转。
未完成的历史任务不标记为已实现；下列事项只有用户重新启动相应工作时再推进。

| 后续优先级 | 范围 | 继续时的验收重点 |
|---|---|---|
| P0 | 稳定性与平台验收 | 调查并行测试资源竞争下的超时；多小时 soak、持续 fuzz、macOS/MSRV/远端 CI 的真实执行证据 |
| P1 | 完整 WAL（CTL-03） | 绑定/锚定、独占追加与同步、决定/重放、持久化幂等回执、逐窗口崩溃测试；现有编解码仅作基础 |
| P1 | 管理权限与资源 API（CTL-01/02/04） | 资源级授权、token 轮换/撤销/审计、明确活动连接与 draining 状态 |
| P1 | TLS 与发布验收（TLS/OPS/REL） | 授权域名外部 CA 验收、平台信任边界、安装升级/迁移、SBOM 与签名产物 |
| P2 | 可选功能扩展（NET/WEB/CGI/OBS） | TTL/SRV、Unix listener、H3/L4/forward proxy、Unix CGI containment、模板与 OTLP；各自独立规划，不阻塞本轮结束 |

详细任务编号、依赖和验收标准仍保留在本文第 4～6 节；历史实施记录用于区分已实现与待办。

最终验证记录（2026-09-09）：

- Windows：`cargo test --workspace --all-features --all-targets --locked --quiet` 退出 0。
- Linux Docker：同一全工作区命令最终退出 0；首次运行在 gatel 宿主出现 14 项 checkpoint/同步/子进程夹具超时（162 通过，51.78 秒），不改代码、超时或并发设置复跑后全工作区通过，宿主 176 项用时 6.37 秒。此前该版本宿主定向回归也通过。失败仍是未解释的环境/时序敏感性证据，不能据复跑通过声称根因已修复，保留为后续 P0 调查项。
- Windows：`cargo clippy -p gatel --all-features --all-targets --locked -- -D warnings`、`cargo run -p xtask --locked --quiet -- check`、`cargo check -p gatel --no-default-features --locked --quiet` 与 `git diff --check` 通过。xtask 仍提示 generation.rs 为 601 行，超过建议值但未超过硬上限。
- 本轮默认全工作区测试不执行四项 ignored 的外部互操作/定时 soak；此前第 283 批的执行证据保留，不冒称本轮再次执行。未运行远端 CI、macOS、多小时长稳或真实生产发布验收。
- 此收尾表示当前范围的开发交付结束，不是 REL-01 完整功能发布或无缺陷保证；未提交/推送 Git，保留现有工作区改动供用户审阅。

第 287 批（2026-09-09，二进制 WAL 记录基础与范围冻结）：

- 新增独立 GATELW01 帧编解码与只读扫描：二进制载荷、连续顺序号、头部/载荷独立摘要、保留字段和类型校验、明确不完整尾部。
- 固定 41 字节的决定载荷绑定意图顺序号和身份，区分 Applied/Rejected，拒绝未知结果及非相邻引用；pending 状态与身份匹配仍由后续事务层负责。
- 新增 64 MiB 实验日志容量预算，开始意图前同时预留意图、决定及两个顺序号，防止软件容量/序号耗尽阻止收尾；不保证物理磁盘空间。
- 新增六项测试覆盖二进制往返、每个尾部截断点、逐字节损坏、重封后的非法头部、容量算术溢出和决定引用边界。未接入磁盘追加或生产请求。
- Windows gatel 全功能全目标测试（宿主 173 项及 CLI）、严格 Clippy、xtask 已通过；最终跨平台及工作区收尾结果另记，不以这些定向检查代替完整工作区验证。

第 286 批（2026-09-09，完整来源 WAL 意图载荷基础）：

- 新增独立 GATELI01 有界二进制编解码，保留完整候选来源，绑定实例、构建、请求身份、原始前置版本和完整前置快照；上限 16 MiB + 208 字节，不修改旧文本日志容量与检查点格式。
- 请求键按来源作用域哈希、大小写敏感且限 1..128 可见 ASCII 字节；文件观察采用独立身份域。错误和 Debug 脱敏，但快照仍含配置敏感信息，校验值不提供存储认证。
- 复用共享来源历史校验，允许语义无效意图；新增五项回归，覆盖上下文、截断、损坏、重封后的非法长度/尾随数据、超大载荷和修订违规。保留 codec 的 Limit/Invalid 错误分类。
- Windows/Linux gatel 全功能全目标测试通过（宿主 167/170 项及各 CLI 目标）；Windows 严格 Clippy、xtask、diff-check 通过。本批未重跑整个工作区。
- 新增设计文档说明原始前置版本不是新启动 CAS 令牌，载荷身份不是回执；明确重试必须在重新观察/分配修订前查询规范请求身份。生产 WAL framing、同步决定、重放、持久化回执与在线轮转仍待接入。

第 285 批（2026-09-09，CTL 完整来源历史约束统一）：

- 模型层新增 Snapshot::validate_successor 与脱敏错误类型，统一来源保留、非零修订、禁止回退及同修订完整节点身份约束；允许更高修订 tombstone 和语义无效意图，不承担授权、大小预算、CAS 或编译职责。
- 检查点校验和 GenerationStore 冷恢复改用共享实现，去掉同修订历史比较中的重复 snapshot 编码和整树克隆；位置、节点顺序与浮点位比较仍保留。未作性能数字承诺。
- 新增来源丢失、零修订、回退、同修订内容/位置改变、浮点符号零、tombstone 与修订跳跃回归；既有检查点/恢复回归继续通过。
- Windows/Linux 的模型、编译和宿主全功能全目标测试通过；Windows 严格 Clippy、xtask、diff-check 通过。本批未重跑整个工作区。
- 持久化设计明确旧 GATEL-TX1 Admin 文本与完整来源快照的语义及容量差距；后续 WAL 必须版本化定义完整来源意图/前置版本/请求身份和结果回执，不以本次共享校验替代生产 WAL 集成。

第 284 批（2026-09-09，CTL 只读管理凭据）：

- 新增可选 --admin-read-token-file，沿用私有文件与 token 校验；读写 token 必须不同，监听绑定前检查。普通与 checkpoint-only 启动路径均装配只读凭据。
- 请求按凭据区分权限，只有精确版本/状态 GET 对只读 token 开放，其他路径/方法默认拒绝；403 在 body、写入槽与配置观察之前返回。写入 token 保留原权限，读写摘要按常量时间校验。
- 新增只读权限矩阵、无 body 轮询、状态不变、写 token 仍可修改、无效/重复 token 和 CLI 参数/文件拒绝回归；真实 checkpoint CLI 覆盖只读查询及未完成 body 写入立即拒绝。
- Windows/Linux gatel 全功能全目标顺序回归通过（宿主 162/165 项，checkpoint_run 各 6 项）；Windows 严格 Clippy、xtask、无默认 feature 编译和 diff-check 通过。本批未重跑整个工作区。
- 仍要求已有写入 token/listen 配对；尚无独立无写凭据监听、动态轮换/撤销、多 token 或资源级授权。完整权限/审计、WAL、幂等回执和在线轮转继续待办。

第 283 批（2026-09-09，显式互操作与稳定性短测）：

- 显式执行默认 ignored 的 60 秒流量/热重载测试，Windows 6,134 轮、Linux 8,547 轮通过；资源计数满足既有断言。Linux RSS 18,408→26,540 KiB 仅作记录，不声称零增长或用轮数比较平台性能。
- 固定缓存 PHP 8.4.25 镜像下，Windows/Linux 各通过真实 FPM 协议与 HTTP GET/POST/HEAD 两项测试；Windows PHP 双向 bcrypt/Argon2id 六组组合也通过。只使用本次示例夹具和测试容器，容器已删除，镜像保留。
- 修复 soak 显式时长格式错误会静默回退 30 秒的验收漏洞：现在在创建资源前拒绝非法/零/溢出输入，输出请求时长，并以 elapsed 比较避免绝对 deadline 加法溢出。未设置时仍为 30 秒。
- Windows 解析回归、soak 严格 Clippy、xtask/diff-check 通过；Linux 修改后的 soak 全目标（含 ignored）三项通过。Windows 60 秒运行发生在时长解析修改前，未据此冒称修改后的 Windows 全量或整个工作区已重跑。
- 四项 ignored 均获得本批明确范围的执行证据，不改变它们的默认门禁属性，也不等同多小时、生产 PHP 配置、Unix socket 或全部协议长期验收。完整目标仍待继续推进。

第 282 批（2026-09-09，全工作区回归与能力台账校正）：

- Windows 与 Linux 执行 cargo test --workspace --all-features --all-targets --locked --quiet，最终均获得完整通过结果；覆盖截至第 281 批的源码。四项显式 ignored 的 PHP/FPM 互操作及长测不计入本批验收。
- 保留首轮失败事实：同机并行两平台全量运行时，Linux 宿主 14 项失败，输出包含检查点期限超时及崩溃子进程停在日志打开阶段；Windows 宿主耗时约 160 秒但通过。Windows 完成后，不改代码、不放宽期限、不改变测试并发参数，Linux 全工作区单独复跑通过，宿主约 6.3 秒。
- 尚未证明并行首轮失败的根因或消除资源竞争下的稳定性风险，不将复跑通过写成修复完成；后续 QA 仍需负载隔离/压力复现与资源证据。
- 对照源码修正旧功能恢复台账：Windows CGI、目录浏览、Admin/CAS/状态/检查点/压缩以及已实现 CLI 不再错误标为未开始。FEATURES 补充 Admin 和持久化 partial 及对应证据，保留旧资源 API、完整 WAL/幂等回执等缺口。
- xtask 与 diff-check 通过。本批主要补全跨模块验证和修正状态清单，未声称所有 roadmap 项完成。

第 281 批（2026-09-09，CTL 诊断读取容量隔离）：

- Admin 处理器为精确 GET 版本/状态查询保留两个独立只读槽；写入及其他请求仍受八槽限制，认证和 Origin 校验在两种容量接纳之前执行。
- 真实 HTTP 回归建立八个未完成 body 的写请求，确认它们持续占槽时版本/状态查询成功、第九个写入返回 503；只读槽也有容量上限，关闭后全部槽归还，未完成 body 不改变配置状态。
- 文档说明只隔离处理器容量，不隔离底层 32 连接槽、CPU 或内核资源，不宣称完整抗 DoS 保证。
- Windows/Linux gatel 全功能全目标测试通过（宿主 160/163 项）；Windows 严格 Clippy、xtask、无默认 feature 编译及 diff-check 通过。本批未重跑整个工作区。
- WAL、幂等回执、在线轮转与其他规划任务仍待完成。

第 280 批（2026-09-09，CTL/OBS 发布与激活进度分离）：

- 结构化状态新增 last_activated_generation 十进制字符串，直接读取宿主已完成激活的代号，不把 GenerationStore 发布视为激活或持久化确认。
- 先读取激活代号再取配置状态，保持报告中的激活代号不超过发布代号；允许并发激活造成暂时滞后，不宣称全局原子快照或当前监听器存活证明。
- 确定性回归持有激活锁，覆盖首次发布 1/激活 0/not-ready 与后续发布 2/激活 1/旧配置 ready；查询不会等待该锁，释放后激活代号前进。未发布和正常启动状态测试同步更新。
- Windows/Linux gatel 全功能全目标测试通过（宿主 159/162 项）；Windows 严格 Clippy、xtask、无默认 feature 编译及 diff-check 通过。本批未重跑整个工作区。
- WAL、幂等回执、在线轮转与其他规划项仍待完成。

第 279 批（2026-09-09，CTL/OBS 结构化管理状态）：

- 新增认证 GET /v1/config/status，提供版本化 JSON：配置 CAS token、字符串 generation、是否已发布、来源差异、脱敏失败阶段/次数、Admin 写入模式、持久化故障锁存及固定去重健康原因。
- 明确发布、激活、readiness 与持久化确认不同；查询 200 不代表健康。配置与健康分别取样，configuration_etag 仅用于配置写入，不作为会随健康变化的状态响应 HTTP ETag。
- 不返回自由文本诊断、来源/资源名称、路径、token 或配置快照；GET 不读取请求 body。认证、Origin、容量、方法、query 与关闭保护均有回归，未发布状态和配置拒绝/恢复亦覆盖。
- 真实 HTTP 检查点保存故障测试补充状态查询，确认 checkpoint 模式、已发布但 persistence_unknown/not_ready，并验证无 token 泄漏。
- JSON 序列化仅作为宿主 admin-api 的可选依赖，并显式更新依赖层规则；低层依赖保持不变。Windows/Linux gatel 全功能全目标通过（宿主 158/161 项）；Windows 严格 Clippy、xtask、无默认 feature 编译及 diff-check 通过。
- 本批未重跑整个工作区；WAL、幂等回执、在线轮转及其他路线图任务仍待完成。

第 278 批（2026-09-09，CTL-03 离线压缩真实进程强杀）：

- 新增独立子进程回归，复用真实压缩磁盘发布函数，在日志同步、检查点同步、发布前、发布后四处暂停并由父进程强杀，Linux 断言 SIGKILL。
- 验证存活期间原目录锁互斥、退出后锁可重新获取、原日志/检查点逐字节不变；发布前没有目标目录，发布后目标可独立读取完整双快照及原发布身份。
- 发布前强杀保留暂存目录，重试不采用或改动残留文件；已发布目标拒绝覆盖。恢复后被拒绝意图仍保留，不把服务快照当作最新意图。
- Windows/Linux 定向回归各 1 项通过（每项含四个真实子进程阶段）；Windows 严格 Clippy、xtask、diff-check 通过。测试有期限及子进程 RAII 清理，未加入生产环境变量故障入口。
- 本批未重跑全工作区，也不是系统断电或全部指令级崩溃窗口验收；WAL、幂等回执及在线轮转仍待实现。

第 277 批（2026-09-09，CTL-03 离线日志压缩导出）：

- 新增显式 journal-compact 命令，以原目录现有独占锁读取，要求检查点完整覆盖日志；拒绝缺失检查点、未决/未覆盖意图（包括拒绝）、不完整尾部和校验错误。
- 完整保留服务/观察快照及原发布身份，将检查点重新绑定到新目录的 binding-only 日志，回收历史记录占用。旧目录不修改、不删除，旧日志序号不是跨目录幂等身份。
- 私有暂存目录内写入/同步三份文件，整体改名后发布，Unix 同步目录；目标已存在则拒绝。改名后错误属于需检查结果的状态，部署切换仍须显式人工完成。
- 新增日志容量回收、拒绝意图保留、可继续追加、目标防覆盖、未覆盖状态拒绝，以及四个写入/发布阶段的错误注入回归。真实 CLI 测试覆盖导出后冷启动、服务有效旧路由、差异标记及新 ETag 修正。
- Windows/Linux 的 gatel 全功能全目标测试通过（宿主分别 154/157 项，checkpoint_run 各 6 项）；Windows 严格 Clippy、xtask、无默认 feature 编译、diff-check 通过。未重跑全工作区。
- 这是可用的离线压缩路径，不是在线轮转或原地多文件切换；WAL、幂等回执、在线轮转、真实强杀全部保存窗口/断电验收仍待完成。原目录须保留至新目录检查/切换完成，强杀遗留暂存目录不自动清理。

第 276 批（2026-09-09，CTL-03 恢复状态与来源差异可见性）：

- GenerationStore 状态新增 observed_differs，在同一读锁内比较最新已接纳修订与发布构建输入；Admin 版本查询返回 X-Gatel-Observed-Differs，与 ETag 同次取样，错误响应不附带此字段。
- 不把差异直接解释为失败：NoOp、准备中和被拒绝意图都可能导致差异。文档明确检查点未保存历史失败诊断/次数，恢复后的 ready 不证明最新意图已验证。
- 覆盖未发布、首次发布、NoOp、刷新发布、拒绝意图恢复，以及真实 CLI 强杀/冷恢复后的 HTTP 状态标记；不输出配置内容。
- Windows/Linux 的 gatel、gatel-plan 全功能全目标测试通过；Windows 严格 Clippy、xtask、diff-check 通过（generation.rs 601 行触发 600 行目标提示，未超过硬上限）。本批未重跑整个工作区；WAL、幂等回执、轮转等仍待完成。

第 275 批（2026-09-09，CTL-03 拒绝意图跨进程恢复与修正）：

- 扩展真实 CLI/Admin 重启回归，200 后提交可解析但不支持的配置并收到 422，验证来源 Version 前进；强杀后 journal-inspect 确认检查点服务/观察快照不同。
- 当前文件损坏时重启仍可访问先前有效 Admin 路由，旧 ETag 被拒绝；使用新 ETag 修正 Admin 内容获得 200，Version 再次前进。
- 原有纯 200 恢复用例保留，共用进程/请求辅助实现。Windows/Linux checkpoint_run 各 5 项通过；Windows 严格 Clippy、xtask、diff-check 通过。
- 未将该场景推论为全崩溃窗口或断电保证；WAL、幂等回执、轮转及其他规划能力仍待完成。

第 274 批（2026-09-09，CTL-03 已确认 HTTP 更新强杀后恢复）：

- 新增真实 CLI/Admin 子进程回归，私有 token 文件认证后提交新路由，收到 200 后强制终止测试进程并确认检查点保留。
- 当前配置文件损坏后启动第二个进程，真实业务 HTTP 仍访问到已确认的 Admin 路由；新启动 ETag 不同，旧 ETag 的 PUT 返回 412。
- Windows/Linux checkpoint_run 集成目标各 4 项通过；Windows 严格 Clippy、xtask、diff-check 通过。测试有子进程 RAII 清理、请求/就绪期限，只终止自身启动的测试进程。
- 不将该场景等同于保存中途全部崩溃窗口、系统断电或幂等回执；WAL、轮转及其他规划能力仍待完成。

第 273 批（2026-09-09，OBS 持久化故障与健康指标）：

- Prometheus 新增 gatel_ready 与固定 7 类 gatel_health_reason gauge，同次健康取样，保留零值；不使用错误详情、资源名或配置内容作标签。
- 单测区分普通配置拒绝的 degraded/ready 与 persistence_unknown/not-ready，并验证标签集合完整、唯一和诊断不泄漏；真实 HTTP 保存故障回归增加 metrics 路由抓取断言。
- Windows 观察层 58 项、定向 HTTP 1 项通过；Linux gatel/gatel-observe 全目标通过（宿主 154 项、观察层 58 项及 CLI 目标）；Windows 严格 Clippy、xtask、diff-check 通过。
- METRICS 文档补充语义和非原子跨指标边界。本批非全工作区或外部 Prometheus 联调；WAL、幂等回执与轮转仍待完成。

第 272 批（2026-09-09，CTL-03 持久化故障真实健康探针）：

- 扩展实际 Admin PUT/保存故障测试，配置独立主机名下的 readiness/liveness 路由，经真实业务 TCP 监听验证健康输出，而非只读内部状态。
- 保存前 readiness 200；失败后 readiness 503、no-store 和静态持久化故障原因，liveness 200；显式重开并恢复已确认双快照后，新 BootId 的 readiness 恢复 200。
- Windows/Linux 定向端到端回归各 1 项通过；Windows 严格 Clippy、xtask、diff-check 通过。未重跑宿主或全工作区全量，不等同于外部负载均衡器联调。
- WAL、幂等回执、日志轮转与其余规划能力仍待完成。

第 271 批（2026-09-09，CTL-03 持久化故障 readiness）：

- 新增稳定健康原因 persistence_unknown：检查点保存/预检返回结果不明，或恢复确认不明时，由宿主锁存本地 readiness 故障；后续内存提交不能清除，不改变 liveness。
- 保持旧服务/连接运行与 readiness 状态分离，不将磁盘/内存可能不一致伪装成普通配置拒绝。HTTP 保存失败回归追加 NotReady、故障原因与 liveness 断言。
- Windows 宿主 151 项、观察层 57 项通过；Linux gatel/gatel-observe 全目标通过（宿主 154 项、观察层 57 项及 CLI 目标）。最后追加的原因代码唯一性断言在 Windows 观察层重跑通过。
- Windows 严格 Clippy、xtask、diff-check 通过，架构/Admin 文档更新。非全工作区或负载均衡器联调验收；WAL、幂等回执与轮转仍待完成。

第 270 批（2026-09-09，可写检查点集成后的全工作区回归）：

- Windows 与 Linux 均完整执行 cargo test --workspace --all-features --all-targets --locked --quiet，并返回 exit 0，覆盖截至第 269 批的检查点 CLI、watcher、HTTP 与重复保存优化，以及其他工作区模块。
- 两端各 4 项显式 ignored 不构成验收；本轮不是 macOS、远端 CI、长期 soak 或断电测试。
- 校准持久化设计文档的剩余旧描述：实例绑定/检查点已进入运行模式，不再统称“日志模块未接入”；区分写入 200 保存语义与尚未提供的幂等查询回执，补正实际启动的恢复分类接入状态。
- xtask 与 diff-check 通过。整体目标仍进行中：WAL、幂等回执、日志轮转及其他规划能力不能由这轮全量通过推定完成。

第 269 批（2026-09-09，CTL-03 稳态检查点避免重复替换）：

- 完整校验日志和现有检查点后比较封装字节，相同则跳过 staging/sync/rename，减少无变化 watcher 轮询的磁盘写入；不缓存跳过读取、权限或损坏检查。
- 阶段回调测试证明相同封装未进入写入阶段；相同 plan 的新生命周期仍完成替换，现有检查点损坏时拒绝且保留原字节。现有来源修订、保存故障及长驻 CLI 退出回归保持通过。
- Windows/Linux 均完整通过 gatel 包 all-features/all-targets（宿主分别 151/154 项，加 CLI/集成目标）；Windows 严格 Clippy、xtask、diff-check 通过。未作吞吐/延迟数值提升声明，未重跑全工作区。
- WAL、幂等回执、日志轮转与完整崩溃/断电验收仍待完成。

第 268 批（2026-09-09，CTL-03 长驻检查点 CLI 故障退出）：

- 新增真实二进制长驻进程回归，HTTP 就绪后验证第二个 CLI 无法取得日志写锁；通过私有测试目录中的非普通文件目标触发保存故障。
- 验证 watcher 错误确实驱动 CLI 以 3 排空退出，stderr 指明检查点 watcher 失败，端口可重新绑定；显式修复路径后再次启动，恢复最后确认的检查点且字节不变。
- 子进程由 RAII 清理并有就绪/退出期限，正常验收路径不依赖 kill。Windows/Linux checkpoint_run 集成目标各 3 项通过；Windows 严格 Clippy、xtask、diff-check 通过。
- 本批未重跑全工作区或宿主全量；不宣称信号、强杀或断电恢复已验收。WAL、幂等回执与轮转仍待完成。

第 267 批（2026-09-09，CTL-03 可写检查点 CLI 模式）：

- 新增显式 run --checkpoint-journal DIR --checkpoint-instance ID，与只读 bootstrap 互斥；通过统一会话完成启动/恢复、文件 watcher、可选 Admin 构造与关闭，不改变默认内存模式。
- watcher 非 CAS 提交错误触发宿主排空并返回退出码 3；启动警告 checkpoint-only 局限。该模式 dry-run 明确会初始化/保存检查点，非只读验证。
- 真实二进制回归覆盖首次保存、损坏当前配置后从检查点恢复、错误实例拒绝且文件不变、参数成对约束与互斥。
- Windows/Linux 均完整执行 gatel 包 all-features/all-targets 测试（宿主分别 150/153 项，加各 CLI/集成目标）并 exit 0；Windows 最小功能宿主 59 项、严格 Clippy、xtask、diff-check 通过。非全工作区验收，也尚未新增该模式的长驻子进程信号/故障退出测试。
- CLI、Admin、检查点设计与事件文档同步。WAL、幂等回执、日志轮转及完整崩溃/断电验收仍待完成。

第 266 批（2026-09-09，CTL-03 检查点 watcher 接线）：

- watcher 循环提取提交回调，普通模式保留行为；新增 reload_checkpoint_until 使用统一会话提交，复用真实文件通知、debounce、目录刷新和兜底轮询。
- 纯 CAS 冲突继续等待观察；其他错误发出 checkpoint_reload_failed 并返回宿主，不自动重试或退回内存写入。事件台账同步更新。
- 真实文件回归验证修改被保存、实际文件保存故障使 watcher 返回 PersistenceUnknown、旧服务继续响应、修复路径后会话仍锁存失败。
- Windows/Linux all-features 宿主分别 150/153 项通过；Windows 最小 feature 59 项、严格 Clippy、xtask、diff-check 通过。非全工作区验收。
- CLI 模式选择与终止策略仍待接线；WAL、幂等回执与日志轮转未完成。

第 265 批（2026-09-09，CTL-03 统一检查点会话）：

- 新增 CheckpointSession，统一持有未使用的 App、检查点写者、待恢复双快照和启动状态；集中提供 startup、文件 reload 和 Admin 构造入口，启动未确认前不开放更新适配器。
- 新建会话从文件启动，已有检查点优先恢复且不读取当前配置文件；成功后拒绝重复启动，恢复准备失败保留快照供重试。
- 集成回归串联真实 HTTP PUT、文件 NoOp、重启前后完整双快照对照、损坏文件不阻止检查点启动、修复后文件修订从恢复基线继续递增，Admin 来源保留。
- Windows/Linux all-features 宿主分别 149/152 项通过；Windows 严格 Clippy、xtask、diff-check 通过。非全工作区验收。
- 会话仍待接入 CLI 与 watcher 循环；WAL、幂等回执、日志轮转未完成。

第 264 批（2026-09-09，CTL-03 HTTP 断连后的提交所有权）：

- 增加真实 TCP 生命周期回归：发布后阻塞检查点保存，激活完成时客户端尚无响应字节且磁盘仍为旧版本；断开客户端并完成 AdminServer shutdown，App 仍持有提交执行锁。
- 释放阻塞并等待应用提交排空后，新检查点保存成功；重开逐字节比较完整服务/观察快照。此测试不宣称断连客户端已获知结果，也不替代幂等查询或崩溃恢复。
- Windows/Linux all-features 宿主分别 148/151 项通过；Windows 严格 Clippy、xtask、diff-check 通过。非全工作区验收。
- 生产统一策略、WAL、幂等回执与日志轮转仍未完成。

第 263 批（2026-09-09，CTL-03 检查点 HTTP 适配）：

- 新增宿主显式构造入口 AdminApi::with_checkpoint，绑定来源级检查点写者，复用认证、限流、正文限额、CAS 与现有响应映射；未改变默认 CLI 模式。
- 真实 TCP 请求验证成功保存的 200、旧版本 412，以及已发布/激活但保存失败时明确提示结果不明的 503，无 ETag/Retry-After；修复文件路径后仍阻止自动重试。
- 关闭并重开后完整比较最后一次 200 的服务/观察双快照，证明失败的新版本没有冒充已保存版本。故障注入仅操作私有测试目录。
- Windows/Linux all-features 宿主分别 147/150 项通过；Windows 严格 Clippy、xtask、diff-check 通过。非全工作区或崩溃验收。
- 生产统一策略与 CLI 开关、WAL、幂等回执、日志轮转仍待完成。

第 262 批（2026-09-09，CTL-03 初始观察失败可重试）：

- 回归复现首次 inline 语法错误被转换为 PersistenceUnknown、导致写者锁存的问题。修复仅豁免版本未变、无服务状态且无任何来源意图的 Rejected，不创建虚假的检查点。
- 测试继续使用同一写者处理缺失文件、文件语法错误，验证版本/日志不变、无检查点且写者就绪；随后修正配置成功启动、保存并通过真实 HTTP 验证。
- 已解析并保留的初始无效来源仍要求恢复协调，既有回归保留。Windows/Linux all-features 宿主分别 146/149 项通过；Windows 严格 Clippy、xtask、diff-check 通过，非全工作区验收。
- 生产持久化策略、WAL、幂等回执与日志轮转仍未完成。

第 261 批（2026-09-09，CTL-03 文件前端检查点入口）：

- 新增 reload_frontend_checkpoint_if：读文件前完成 CAS 与写者预检，复用普通 reload 的观察/来源合并/资源刷新阶段，以及已有激活后检查点收尾。
- 真实文件与 HTTP 回归覆盖 stale 不推进文件修订缓存、NoOp 保存新文件观察、文件 reload 保留被拒绝的 Admin 意图、失败锁存阻止继续解析；重开后完整比较服务与观察双快照编码。
- Windows/Linux all-features 宿主分别 145/148 项通过；Windows 严格 Clippy、xtask、diff-check 通过。未重跑全工作区。
- 此入口供后续统一 watcher/startup 策略接线；生产模式、WAL、幂等回执与日志轮转仍待完成。

第 260 批（2026-09-09，CTL-03 来源检查点取消与关闭回归）：

- 增加来源级真实 App 回归：发布后取消调用者，在检查点锁被阻塞期间完成激活并启动 shutdown；验证旧检查点未提前替换、关闭等待收尾、新来源写入关闭后不可用。
- 释放锁后检查保存完成、监听关闭、准入名额归还；重开检查点，与宿主服务/观察双快照完整编码逐字节对照。使用锁和通道控制顺序，不以 sleep 猜测时序。
- Windows/Linux all-features 宿主分别 144/147 项通过；Windows 严格 Clippy、xtask、diff-check 通过。本批验证进程内生命周期，不代表崩溃、断电或全工作区验收。
- 生产统一持久化策略、WAL、幂等回执与日志轮转仍未完成。

第 259 批（2026-09-09，CTL-03 来源授权检查点写者）：

- 新增内部 checkpoint_source_writer：签发时绑定来源与检查点策略，每次提交不能更换来源、修订或持久化写者。复用现有来源合并/CAS/解析路径，保持最小 feature 构建无检查点依赖。
- 提取完整 Snapshot 与来源写入共用的 finish_checkpoint，在 owned execution 中完成激活检查、完整取样、保存与失败锁存，不复制另一套提交协议。
- 新增真实宿主/磁盘/HTTP 回归，覆盖文件与 Admin 来源共存、CAS 拒绝、语法拒绝、编译拒绝的双快照、失败后阻断以及关闭重开后的完整来源编码对照。
- Windows/Linux all-features 宿主分别 143/146 项通过；Windows 最小 feature 59 项、严格 Clippy、xtask 与 diff-check 通过。本批不是全工作区回归。
- 该能力仍是内部集成入口；普通生产 Admin 与 watcher 尚未切换持久化策略，预写日志、幂等回执、压缩/轮转仍未完成。

第 258 批（2026-09-09，CTL-03 日志容量边界）：

- 修正容量拒绝错误：不再提示保存检查点即可恢复追加，而是明确需要尚未实现的日志压缩/轮转。
- 扩展 16 MiB 容量回归：预留结果记录、拒绝超限意图、写入最终结果、保存并读取检查点、关闭重开后再次拒绝追加；验证拒绝不制造未决意图，检查点不改变日志字节，也不释放容量。
- Windows/Linux all-features 宿主分别 142/145 项通过；Windows 严格 Clippy、xtask 与 diff-check 通过。本批未重跑全工作区，最近完整基线见第 257 批。
- 压缩/轮转和生产可写模式仍待完成；不能通过手动删除 intent.log 绕过容量限制，否则会破坏检查点的日志前缀绑定。

第 257 批（2026-09-09，全工作区回归与持久化文档校准）：

- Windows 与 Linux 均完整执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，返回 exit 0；显式 ignored 测试不构成验收。该轮覆盖截至第 256 批的恢复、检查点、锁修复以及其他工作区模块，不是仅宿主定向回归。
- admin-journal 增加内部能力/生产入口状态表，分开已实现宿主边界与剩余生产集成；修正原子取样、只读启动、内部检查点收尾仍被误标成未实现的旧表述。
- checkpoint-envelope 同步更新内部入口和只读 bootstrap 状态，区分封装层与真实 App/HTTP 验证。明确现有检查点不截断/轮转日志，不能消除 16 MiB 日志容量上限；压缩/轮转仍待实现。
- xtask 与 diff-check 通过。生产可写模式、预写日志/幂等回执等仍未完成，全量通过不代表总目标完成。

第 256 批（2026-09-09，CTL-03 恢复冲突与准备失败重试）：

- 确定性测试复现恢复 CAS 拒绝前已预留写者的缺陷：错误 App 的请求返回 Conflict 后，合法恢复被 WriterUnavailable 阻止。修复为先检查冷启动条件与完整 Version，再预留 BootId，失败请求不抢占归属。
- 同一回归保持原 App 监听端口，用真实 bind 冲突触发 PrepareFailed，验证无新 generation、观察意图保留、写者未确认、磁盘检查点不变。停止原 App 后按新 Version 重试恢复成功、确认写者并验证 HTTP 200，恢复本身仍不改写磁盘。
- Windows/Linux 宿主分别 142/145 项通过，包含上一批追加的竞争恢复归属断言；Windows 严格 Clippy、xtask、diff-check 通过。
- 生产可写启动模式、全来源日志编排和幂等回执继续实施，未宣称 CTL-03 完成。

第 255 批（2026-09-09，CTL-03 原子恢复编排）：

- 新增内部 App::recover_checkpoint_if，将来源预检、恢复/激活与写者确认保持在同一 owned execution 中；确认使用阻塞收尾且不改写磁盘。与只读恢复抽取共用 recover_snapshot_if，避免资源准备和遥测分叉。
- 恢复前先验证完整来源并预留 BootId，防止两个 App 同时使用 pending 写者；确认失败明确区分已发布但未确认，不伪装成未应用。
- 重启/HTTP 集成测试改用新入口，验证错误输入在发布前拒绝、激活暂停时仍禁止写入、取消恢复等待者后确认继续完成、随后 NoOp 检查点提交成功。补充相同来源的竞争 BootId 不能预留/确认断言。
- Windows/Linux 宿主分别 141/144 项通过；最后追加的竞争断言在 Windows 覆盖，尚未同步 Linux。生产 writable CLI、全来源持久化策略和幂等回执继续实施。

第 254 批（2026-09-09，CTL-03 已有检查点恢复确认）：

- CheckpointWriter 重开已有检查点后进入 recovery-pending，禁止 bind/save，直到 confirm_recovery 验证未 sealed 的完整 serving/observed capture 与磁盘来源一致。使用 snapshot codec 的全字节域分离指纹，确认后绑定恢复 App 的 BootId；不能仅凭同修订号、同计划或重开句柄继续覆盖。
- 新增回归覆盖丢失被拒绝意图、错误服务配置、相同修订但观察内容不同、未确认直接写入、正确恢复后写入与一次性确认。真实 App 重启/HTTP 测试也执行恢复确认。
- Windows/Linux 宿主分别 141/144 项通过；Windows 严格 Clippy 通过。来源一致性不等于激活或健康证明，生产持久化策略与幂等回执仍待实施。

第 253 批（2026-09-09，CTL-03 写者生命周期归属）：

- CheckpointWriter 在首次 bind/save 绑定一个 BootId，宿主提交预检在观察/发布前绑定，保存再次校验 capture 的 BootId。防止两个 App 通过同一 Rust 写者句柄绕过彼此独立的 execution gate。
- 跨生命周期访问不修改磁盘或外来 App 的 Version/观察意图，也不污染原合法写者；测试验证拒绝后原 App 仍能发布、落盘并返回真实 HTTP 200。既有编码失败锁定测试改用同生命周期的未发布 capture，保持其原本覆盖点。
- Windows/Linux 宿主分别 140/143 项通过，Windows 严格 Clippy、定向新增断言、xtask、diff-check 通过。
- BootId 绑定是内存能力约束，不替代磁盘 instance 身份或恢复确认；生产持久化策略、预写日志及幂等回执继续实施。

第 252 批（2026-09-09，CTL-03 检查点提交的真实 I/O 与恢复验证）：

- 新增非普通检查点目标触发的真实文件系统保存失败测试：确认配置已经发布/激活、HTTP 200 仍服务新版本，但返回 PersistenceUnknown；移除障碍后同写者仍拒绝下一次修改，Version 和日志保持不变。此测试覆盖目标检查失败，不冒称实际 fsync/断电失败。
- 新增 NoOp 提交落盘测试：运行 generation 不变，但检查点观察修订推进到 2，服务构建来源保持修订 1。
- 取消等待者的测试进一步重开磁盘快照、创建新 BootId 的 App 并调用 recover_configuration，验证恢复后真实 HTTP 正文，而非仅验证文件存在。
- Windows/Linux 宿主分别 139/142 项通过；Windows 严格 Clippy、xtask、diff-check 通过。生产 CLI/全来源持久化策略、预写日志和幂等回执仍未完成。

第 251 批（2026-09-09，CTL-03 写锁所有权修复）：

- 新增 Linux 确定性回归：保留锁文件 try_clone 副本、销毁 Store 后立即重开。旧实现稳定失败（would block），证明仅 close 原描述符不足以结束 flock 所有权；这模拟 fork 至 exec 前仍存活的共享描述符，不宣称已追踪到上一批偶发失败的实际进程。
- 新增 WriterLock RAII 守卫，在锁成功取得后立即建立，覆盖正常关闭与打开后续错误路径；拥有者析构显式 unlock，PID 检查防止 fork 子进程误解父锁。解锁失败仍由 close/OS 释放兜底，不增加重试或延长测试期限。
- 回归进一步验证旧副本随后关闭不会破坏新写者独占锁。Windows/Linux 宿主分别 137/140 项通过，包含既有日志修复及真实 kill 测试。
- 运行期持久化与幂等恢复仍未完成，本批修复的是可复现的文件锁生命周期缺陷。

第 250 批（2026-09-09，CTL-03 激活与取消边界）：

- 新增三项检查点提交测试：初次配置拒绝时无可恢复服务快照、关闭的生命周期拒绝确认未激活发布、等待者取消后仍完成激活/保存并可重开验证完整服务快照。关闭状态通过持锁注入，明确不是穷举真实 shutdown 调度顺序。
- 测试目录按字段析构顺序在 App/写者之后清理，避免 Windows 文件句柄仍打开时删除失败。
- Windows 宿主 137 项通过；追加重开断言后定向 3 项通过，严格 Clippy、xtask、diff-check 通过。
- Linux 宿主 138 项通过、1 项失败：既有 explicit_tail_repair_does_not_clear_unknown_outcome 在 admin_recovery_tests.rs:186 重开 Journal 时报告 writer lock would block；新增三项通过。此次不能称为 Linux 宿主全绿，需继续排查锁释放与并行子进程继承窗口。

第 249 批（2026-09-09，CTL-03 完整快照检查点提交入口）：

- 新增内部 App::reload_checkpoint_if，统一 Version CAS、写者预检、配置发布/激活和 blocking 检查点保存。保存失败或缺少已激活服务快照返回 PersistenceUnknown；后者也锁定写者要求协调。
- 已有磁盘/真实 HTTP 集成测试改用该入口，追加过期 CAS 不写盘、写者失败时内存 Version/磁盘均不变的验证。Stale 仅在 Version 未变时跳过收尾保存，保留关闭竞争中已观察意图的处理路径。
- Windows/Linux 宿主分别 134/136 项通过，Windows 严格 Clippy、xtask、diff-check 通过。
- 尚未接管生产 watcher/Admin/CLI，未提供预写意图日志和幂等回执；初次无可恢复服务配置时不会承诺成功检查点，CTL-03 继续实施。

第 248 批（2026-09-09，CTL-03 宿主检查点失败停止）：

- CheckpointWriter 增加失败锁定：任意保存失败（包括编码/验证未触盘）后禁止同句柄后续写入，必须关闭重开并核查；成功写入才清除进行中的失败标记。不能只依赖底层 I/O poisoned，因为宿主内存可能早已更新。
- 增加 ensure_ready 供宿主执行门内发布前预检，它不是写盘成功预留，也不能授权自动重开覆盖。
- 新增回归验证编码失败后旧日志/检查点字节不变、后续有效覆盖仍被拒绝、独占锁保持到关闭、重开恢复原服务快照。Windows/Linux 宿主分别 134/136 项通过（Linux 包括上一批新增的真实 HTTP 断言），Windows 严格 Clippy、xtask、diff-check 通过。
- 生产运行期写盘编排和错误恢复尚未完成；这不是 durable ACK 或幂等接口。

第 247 批（2026-09-09，CTL-03 收尾与检查点连接验证）：

- 增加内部 CheckpointWriter 能力，只开放受检打开/完整检查点保存，不向 App 暴露原始日志追加或截尾。仅接受初始化状态或已覆盖检查点，返回服务/观察双快照供宿主先恢复；仍由宿主保证恢复后才覆盖、全来源串行化及错误分类。
- 集成测试通过实际 App 发布/激活和 blocking completion 写入检查点，随后保留被拒绝的修订；重开磁盘验证服务 rev1 与观察 rev2，并在 Windows 追加真实 HTTP 200/旧正文断言。竞争打开被拒绝。
- 另一测试覆盖检查点之后未决/Applied/Rejected 三种日志状态，全部拒绝接管，日志/检查点字节保持不变且释放锁，避免覆盖未协调的意图。
- Windows/Linux 宿主分别 133/135 项通过；追加 HTTP 断言后 Windows 定向测试通过，尚未在 Linux 重跑该追加断言。Windows 严格 Clippy、xtask、diff-check 通过。
- 未增加 CLI 持久化开关，未将 Admin 200 改成落盘承诺；运行期意图日志与检查点统一提交、幂等回执仍待实施。

第 246 批（2026-09-09，CTL-03 关闭期间的阻塞收尾）：

- 新增内部 reload_task_persisting，串行事务最多提交一个阻塞收尾 worker，沿用 owned job 的准入和顺序锁，不因 shutdown 关闭新 configuration_work 而跳过已接收事务的收尾。回调在 blocking 线程执行，不持有 generation/lifecycle 锁。
- 两项确定性测试覆盖关闭后执行、调用者取消、shutdown 等待、名额归还和 panic 传播。Windows/Linux 宿主分别 131/133 项通过，Windows 严格 Clippy、xtask、diff-check 通过。
- 这仍是宿主编排入口，不是已启用的磁盘写者；持久化错误分类、检查点与日志提交顺序、进程崩溃恢复继续实施。

第 245 批（2026-09-09，CTL-03 宿主收尾边界）：

- reload_task 委托给内部 reload_task_finishing，所需 activation 后执行结果收尾，期间持续持有 owned execution gate 与准入；普通入口使用原样返回结果的空收尾，不改变现有内存成功语义。
- 收尾可以接收 work 错误并返回 PersistenceUnknown；排队后 sealed 时不调用 work/finish。取消请求等待者不会丢弃已接收收尾，后续配置任务等待收尾完成。
- 新增取消/排序/名额归还与错误返回测试；强化现有真实激活测试验证收尾顺序，并验证关闭时不误调用收尾。Windows all-features 宿主 129 项、Linux 131 项通过，Windows 严格 Clippy 通过。
- 本批没有接入生产日志写入或回执。收尾时关闭竞态、有界写盘执行、全来源检查点及崩溃后的幂等恢复仍需后续实现；CTL-03 保持未完成。

第 244 批（2026-09-09，Linux 全量复核与崩溃夹具诊断）：

- 三类进程 kill 夹具增加仅测试使用的静态阶段日志：打开日志、写入意图/决定、保存旧/新检查点及 Written/Synced/BeforeRename/Renamed/DirectorySynced。原有失败信息会带出最后阶段，避免只有 running 1 test 而无法区分子进程启动、持久化和就绪通知；不打印配置、密钥或用户路径。
- 保留原 10 秒阶段期限、全部 fsync 和真实 kill/锁竞争/恢复断言。Linux 宿主 129 项、Windows 宿主 127 项通过，但原来的三项超时未复现，根因未证明，不能称为生产持久化问题已经修复。
- 更新验证镜像中的相关源文件后，Linux `cargo test --workspace --all-features --all-targets --locked --quiet` 完整返回 exit 0；4 项显式 ignored 不构成验收。Windows 宿主严格 Clippy、xtask check、diff-check 通过。
- CTL-03 的运行期日志编排、持久化 ACK 和幂等恢复仍待实现；全量测试通过不改变这些功能的未完成状态。

第 243 批（2026-09-09，完整回归与缓存验证夹具）：

- Windows 全工作区回归在日期缓存重验证测试超时后提前停止。该夹具将多次关闭源站后的连接拒绝等待叠加到同一 15 秒预算，定向原版耗时 8.25 秒；单次重跑通过不构成原失败已解决的证据。
- 非法 304 后保持源站监听，验证下一次确实收到不带旧条件头的请求并返回显式 no-store 503；同时断言响应正文且不得命中旧缓存。成功路径仍关闭源站证明缓存命中，每个日期场景显式停止网关。未延长超时或修改生产缓存行为。
- 修改后的 Windows 定向 3 项通过（0.08 秒），对应严格 Clippy 与 diff-check 通过；随后 `cargo test --workspace --all-features --all-targets --locked --quiet` 完整返回 exit 0（按配置忽略的测试不构成验收）。输出部分截断，因此不统计本轮精确总数。
- 先前 Linux 全工作区运行已失败：宿主 126 项通过、3 项崩溃注入测试未及时到达阶段（binding applied、checkpoint first-staged、journal synced）。原因尚未确定，不能将之前定向通过当作本次全量通过；CTL-03 仍未完成。

第 242 批（2026-09-09，CTL-03 持久化错误协议）：

- SourceWriteError 增加独立 PersistenceUnknown，明确内存可能已更新；计划层纯内存 writer 不产生该错误。不能借用 Conflict/Observation/Unavailable 暗示未应用。
- 抽取 Admin 写结果映射，未知持久化结果返回 503 与静态核查提示，不附 ETag/Retry-After。新增响应回归覆盖状态、正文、no-store 和禁止暗示自动重试；正常内存成功仍为 applied in memory。
- Windows 计划层 233 项通过；Windows/Linux 宿主与 reload 回归分别 129/131 项通过，Windows 严格 Clippy、xtask、diff-check 通过。
- 本批是未来运行期写盘必须遵循的错误契约，尚无生产发出路径，不宣称 durable ACK、幂等或持久化已完成。

第 241 批（2026-09-09，CTL-03 引导生命周期验证）：

- 新增真实子进程测试，执行 run 启动路径而非只做 dry-run：从检查点持续响应 HTTP，竞争写者在服务期间不能取得日志锁，强制 kill/wait 后可重开检查点且日志/检查点字节不变；Unix 检查 SIGKILL。
- 用例明确区分实际运行函数与命令行可执行文件测试、进程终止与断电；不将只读锁释放视为运行期持久化或优雅关闭已验收。
- Windows/Linux 宿主与 reload 定向回归分别 128/130 项通过；Windows 严格 Clippy、xtask、diff-check 通过。生产写入、后续日志恢复与幂等回执仍未完成。

第 240 批（2026-09-09，CTL-03 部分）：

- run 新增成对 --bootstrap-journal / --bootstrap-instance（admin-api feature），以只读方式从完整 GATELC02 检查点引导。启动前校验身份/兼容性/日志覆盖，拒绝所有后续记录、缺失和损坏，不退回根 KDL；校验先于管理监听创建。
- 独占日志锁保留到服务关闭，双快照移动到 owned 恢复任务，不在 reactor 深复制；后续 watcher/Admin 仍是内存更新，启动发明确警告，不宣称 durable mode。新增启动失败的显式 App shutdown。
- 新增两项内部启动测试与一项实际 CLI 失败路径测试：成对参数/缺失不创建/未覆盖拒绝/锁保留，缺失根 KDL 的真实 run dry-run 能引导并关闭监听且不修改存储，以及缺失 bootstrap 先于管理凭据读取失败。修复两个 flatten Args 同名的 Clap group 冲突。
- Windows/Linux all-features 宿主、CLI、reload、生成帮助测试分别 134/136 项通过；Windows 最小 feature 61 项、严格 Clippy、xtask、diff-check 通过。非全工作区或长期运行验收。
- 此入口仅引导已有完整检查点，不创建初始检查点、不重放后续记录、不保存后续更新；生产运行期持久化、后续恢复策略和幂等回执仍须完成，CTL-03 保持实施中。

第 239 批（2026-09-09，CTL-03 诊断完善）：

- journal-inspect 增加双快照来源摘要：服务/观察来源数量、空观察贡献数量、观察修订是否不同。无检查点显示 none；不输出来源名称、配置正文或内容指纹，不改变退出码。
- 新增 NoOp 空贡献测试，扩展正常检查点、语义拒绝检查点、缺失检查点测试，防止将差异当成非法配置或将缺失当成零。摘要仅描述检查点，不冒充后续日志重放结果。
- Windows/Linux 宿主与 CLI 定向测试分别 126/128 项通过；Windows 严格 Clippy、xtask、diff-check 通过。本批是诊断能力改进，生产启动与持久化回执仍未完成。

第 238 批（2026-09-09，CTL-03 部分）：

- 修复恢复后的文件修订衔接：reload 以读取前捕获的完整准入意图为基线，单文件/多根文件按来源选择其与本地缓存中较新修订，相同修订使用准入状态。恢复到高修订后不再从 1 开始并被判过期；较新的未准入本地观察也不回退。
- 新增真实 App 恢复后文件重载测试，分别覆盖单根/多根：服务 rev40、拒绝意图 rev41 后读取文件推进至42，下一次变更43，未变额外来源保留70。新增基线选择测试，扩展多根读取失败测试验证高修订恢复时仍不部分推进缓存。
- Windows/Linux all-features 宿主回归分别 124/126 项通过；Windows 最小 feature 56 项、严格 Clippy、xtask、diff-check 通过。Linux 本批 reload 两项用时 6.30 秒，未据此作性能结论；非全工作区验收。
- 修订衔接已落地，但 run 磁盘选项、日志后续记录合并和重启期间文件变更应用策略仍需宿主编排；CTL-03 未完成。

第 237 批（2026-09-09，CTL-03 部分）：

- App 新增内部 recover_configuration 高层入口，将冷启动双快照事务接入现有有界准入、owned execution gate、blocking 资源准备与 activation。普通 reload 与恢复抽取共用 prepare_runtime，避免资源组装分叉。
- 新增 3 项宿主测试：真实 HTTP 返回恢复后的服务配置并保留无效观察意图；非法双快照不发布/激活；在 activation 暂停时取消等待者，已接收恢复仍完成并释放队列名额。已有发布状态明确拒绝再次恢复。
- Windows/Linux all-features 宿主回归分别 122/124 项通过；Windows 最小 feature 54 项、严格 Clippy、xtask、diff-check 通过。本批没有全工作区或外部 CA 验收。
- 尚未连接 run 的磁盘选项、日志后续记录策略及 watcher 修订缓存恢复，不宣称生产启动恢复完整或 HTTP 已持久化；CTL-03 继续实施。

第 236 批（2026-09-09，CTL-03 部分）：

- 新增冷启动 `recover_if` 事务：先验证服务/观察双快照的来源历史，再原子准入观察来源，使用服务来源走现有 normalize/compile/prepare/commit。避免先发布服务再校验恢复意图；保留不同的服务 provenance 与 observed head。
- 仅允许尚未发布 generation 的 store；prepare 失败保留意图但不发布 runtime，可按新 Version 重试。普通发布、封闭和跨 BootId 检查继续生效，不提供在线回滚或绕过 activation 的承诺。
- 新增 5 项计划层测试，覆盖双快照、非法输入零准备/零观察、失败重试、运行中/跨 boot/seal 拒绝与 prepare 内竞争；磁盘往返测试改用冷启动事务验证服务计划与完整观察意图。
- Windows/Linux 计划层各 233 项通过；宿主回归分别 119/121 项通过，Windows 严格 Clippy、xtask 通过。本批不代表整个工作区或实际启动流程已完成。
- 宿主 App 的启动编排、日志后续记录合并、运行期持久化和回执仍待接通；CTL-03 保持实施中。

第 235 批（2026-09-09，CTL-03 恢复并发验证）：

- 在意图恢复的锁外校验完成、最终提交之前加入私有确定性测试接缝；生产入口使用空回调，不改变恢复语义，不把回调暴露给宿主。
- 新增 2 项测试覆盖三个精确交错：普通发布抢先、同来源修订的资源 refresh 抢先、seal 抢先。验证最终完整 Version/封闭检查拒绝迟到恢复，且不替换赢家 runtime、不回退来源、不误写故障状态。测试不依赖睡眠或竞速概率。
- Windows/Linux 计划层各 228 项通过，Windows 计划层严格 Clippy 和 xtask 通过。本批补足恢复提交窗口的证据，不等同于生产启动恢复或全工作区验收。
- 下一步仍需把双快照和日志结果接入实际宿主启动流程；整体目标和 CTL-03 未完成。

第 234 批（2026-09-09，CTL-03 部分）：

- 新增可信宿主 `restore_intent_if` 入口：恢复完整观察来源但不 normalize/prepare/发布，保留服务 runtime、generation 和新 BootId。恢复过程中前后核对完整 Version，遵循来源白名单、容量、修订历史与 sealed 限制；混合输入失败不部分更新。
- 恢复推进的 head 能使落后的准备候选失效；相同修订内容必须连同树和来源位置一致。树复制、验证与退休来源释放都在锁外，接口不暴露给 Admin HTTP，也不重建历史故障状态或承诺 activation。
- 新增 4 项计划层回归：拒绝意图恢复后的来源组合、混合非法输入原子拒绝、tombstone 与 prepare 竞态、容量/白名单/跨 BootId/封闭状态。扩展磁盘检查点测试，在新 store 中先准备服务来源，再恢复无效最新意图，验证服务 Arc 不变且 BootId 更新。
- Windows/Linux 计划层各 226 项通过；宿主回归分别 119/121 项通过，Windows 严格 Clippy、xtask、diff-check 通过。本批不是全工作区或生产启动恢复验收。
- 启动协调、日志后续意图与双快照合并策略、运行期持久化顺序、幂等回执和轮转仍未完成；CTL-03 保持实施中。

第 233 批（2026-09-09，CTL-03 部分）：

- 检查点升级为 GATELC02，磁盘保存入口改为只接受 ConfigurationCapture，同时编码已发布来源和最新完整观察来源。两份快照分别限 16 MiB，总上限 32 MiB + 224 字节；旧实验 GATELC01 明确拒绝，不静默迁移或覆盖。
- 服务来源重编译确认计划；观察来源可包含语义拒绝或空贡献，不强求编译通过。校验来源不得遗漏/修订倒退/为零，相同修订必须完整内容与来源位置一致，长度字段和两份编码均有界验证。
- 新增 4 项回归，覆盖拒绝/NoOp/tombstone、磁盘重开、正确校验下的来源篡改/修订倒退/非法长度、未发布 capture 与旧格式拒绝；既有五阶段真实进程 kill 测试改用同时含服务来源和不同拒绝意图的新旧检查点，验证恢复完整对应版本。
- Windows 122 项、Linux 124 项宿主/CLI/reload 定向测试通过；Windows 严格 Clippy、xtask、diff-check 通过。本批不是全工作区、断电或生产 HTTP 持久化验收。
- 双快照已写入内部磁盘存储，但生产 reload、启动恢复、最新意图重新准入、幂等回执和轮转仍待接通；CTL-03 保持实施中。

第 232 批（2026-09-09，CTL-03 部分）：

- 新增计划层 `capture_configuration()` / 私有字段 `ConfigurationCapture`，一次锁内固定完整写版本、已发布 generation、所有最新来源意图和 sealed 状态，树在锁外复制。避免持久化宿主分别读取 current 与 observed 时组合不同瞬间的数据。
- 服务来源和最新观察来源分别只读暴露；NoOp、拒绝及 prepare 中的意图不覆盖旧发布来源。Debug 不输出配置、来源名或 runtime；捕获不等于等待候选结束、activation 成功或持久化完成。
- 新增 4 项回归测试，覆盖首次失败、NoOp/拒绝、封闭状态、多来源空贡献、对象释放后的固定快照、prepare 暂停时取样，以及 128 次并发发布与 1024 次取样的一致性。
- Windows/Linux 计划层各 222 项通过；宿主定向回归 Windows 115 项通过，严格 Clippy、xtask、diff-check 通过。本批不是全工作区或性能验收。
- 当前 GATELC01 仍仅存已发布来源；后续须使用该原子输入扩展双快照检查点并接入宿主执行区间，不能把本接口当作全来源恢复已经完成。

第 231 批（2026-09-09，CTL-03 部分）：

- 接入 feature-gated `journal-inspect --directory ... --instance ...` 离线命令，复用完整恢复检查，不构建 App、不准备运行时或监听；输出只含静态分类、序号和决定，不包含配置正文、实例或路径。
- 新增 existing-only 打开模式：要求已有锁和日志，空/未绑定日志拒绝认领，不创建或同步文件；日志只读，检查句柄在追加、截尾与检查点保存前统一拒绝写入。
- 明确退出码：0 检查点覆盖日志（可能保留拒绝意图），1 需初始化/协调，2 用法错误，3 检查或输出失败。0 不承诺恢复完成、运行健康或 HTTP durable ACK。
- 新增 4 项单测及真实可执行文件用例，覆盖缺失目录/文件、空日志、身份/锁冲突、未知意图/短尾保留、检查点分类、禁止误写、脱敏与 feature 开关。Windows 与 Linux 的 all-features 定向测试分别 121 / 123 项通过；Windows 严格 Clippy、最小 feature 定向测试 56 项、xtask、diff-check 通过。本批不是全工作区验收。
- 启动恢复、全来源持久化协调、幂等回执与轮转仍未完成；CTL-03 保持实施中。

第 230 批（2026-09-09，CTL-03 部分）：

- 新增只检查、不修改/应用的 Journal 恢复分类，区分绑定初始化、检查点覆盖当前日志、保留后续拒绝意图，以及短尾/未知结果/缺失检查点/检查点后已有成功应用等待协调状态。
- 返回服务检查点与最新/最后应用意图的独立证据，最后一次拒绝不能掩盖此前的成功应用；短尾修复不能清除未知结果。损坏检查点直接报错，不回退成首次启动。
- 新增 5 项测试覆盖状态转移、重开、拒绝意图保留、缺失检查点、短尾与未知共存及检查不修改文件。Current 只代表日志覆盖关系，仍需准入/环境/资源准备/激活；尚未接入生产启动或回执。
- 验证：Windows 宿主 bin + run_reload 111 项、Linux 113 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `9a9e54b88d29845ac968a8b1e0c3a16920897baed6597a206162283e147a2c59`；不是全 workspace 或实际启动恢复验收。

第 229 批（2026-09-09，CTL-03 / RT-04 部分）：

- App 可写 GenerationStore、同步 attempt/attempt_snapshot_if、attempt_async 与 activate 阶段改为事务模块私有。Admin、watcher、CLI 改读 configuration_status 副本和关闭状态，不再持可写 store；测试需要的已发布对象只通过 cfg(test)+admin-api 只读检查口取得。
- serve 前端测试改走 owned reload，不能拆开调用发布/激活；外部高层源码写者保留原有来源绑定、CAS、容量和完整执行互斥。没有增加新依赖或扩大默认 feature。
- 这是 Rust 模块边界收敛，内部宿主代码/生命周期测试仍能调用私有阶段，独立 gatel-plan GenerationStore API 不变。检查点仍未进入 reload 执行区间，生产持久化、回执及启动恢复继续未完成。
- 状态读取接口集中在 app_health 模块，避免继续扩张 app.rs。全功能验证：Windows 宿主 bin + run_reload 106 项、Linux 108 项通过；最小功能：Windows/Linux 各 51 项通过。Windows 全功能/最小功能严格 Clippy、xtask、diff 检查通过。最终 Linux 镜像 `c31410fc5dd4962640e25191c6cbd8b7ec957a19caa94477952196ac4134b3b6`；不是全 workspace 或生产持久化验收。

第 228 批（2026-09-09，CTL-03 / RT-04 部分）：

- 高层 reload 新增完整执行互斥，覆盖 owned task 的观察/编译/发布直至 activation 结束；不再只依赖在激活前已释放的 blocking worker 槽。独立任务持有执行权，调用者取消不释放队列容量或提前放行下一次重载。
- 排队任务取得执行权后再次检查 sealed，shutdown 不会执行尚未开始的工作闭包。执行异常返回仍释放互斥和 admission；保留原有 8 个总任务上限、CAS 和 shutdown drain。
- 新增 3 项测试，验证发布后激活阻塞仍保持执行排他、取消的排队调用继续由宿主负责、shutdown 拒绝整个等待队列以及错误释放执行权。
- 持久化保存尚未接入此执行区间，低层 attempt/attempt_async/attempt_snapshot_if 与直接 GenerationStore 调用仍是可信独立入口，不承诺被此互斥保护；生产持久化接入前必须收敛可写入口。串行等待不提供硬超时或阻塞线程强制取消。
- 验证：Windows 宿主 bin + run_reload 106 项、Linux 108 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `6eae0a9374042266b4d858158cb7d838cc6f45444ddd755ebc711bce565326e4`；不是全 workspace、生产持久化或性能验收。

第 227 批（2026-09-09，CTL-03 / QA-04 部分）：

- 新增检查点真实子进程 kill 测试，覆盖首次写入（临时文件同步后、rename 后）及替换（临时文件同步后、rename 后、目录阶段后）五轮，终止前验证写锁排他性，终止后重开检查正式文件完整性及日志前缀关联。
- 验证 orphan 临时文件在恢复时不被采用、后续保存时不被擅自清理；日志字节保持不变，旧/新检查点分别引用序号 1/3。Linux 额外检查 SIGKILL。
- 本批没有改变生产路径；进程终止不是断电、满盘或系统 sync 失败模拟。全来源提交协调、生产恢复入口及持久化回执仍未完成。
- 验证：Windows 宿主 bin + run_reload 103 项、Linux 105 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `a91c1a06b76a1b0b3651e8b498cbffba780141832c4f560d6a098e303fb4a904`；不是全 workspace 或生产恢复验收。

第 226 批（2026-09-09，CTL-03 部分）：

- Journal 内部提供 checkpoint.bin 保存/加载，复用持续日志写锁和固定实例身份；同目录 create_new 私有临时文件、write_all/sync_all/close/rename，Unix 同步目录。目录在初次检查前固定绝对路径。
- 读取有大小/类型/权限/日志关联/重编译验证；保存先验证旧检查点，拒绝静默覆盖损坏、不安全或不兼容材料。写阶段失败 poison 写者；替换后错误不伪装为未替换，普通错误展开清理本次临时文件。
- 新增 Windows 4 / Linux 5 项磁盘测试，覆盖替换/重开、不改日志、五阶段失败、未决意图、损坏/超限/非普通文件；Unix 额外覆盖私有权限与符号链接。强制 kill/断电、真实满盘和 sync 系统调用失败仍需验证。
- 生产 CLI/HTTP 接入、全来源发布与持久化协调、启动恢复和日志压缩仍未完成。Journal 打开本身尚不自动加载检查点；后续恢复入口必须显式加载且在失败时阻止开放监听。
- 验证：Windows 宿主 bin + run_reload 102 项、Linux 104 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `1398a9e57f8d45b77539ffda0a669e8cc3d47fd60e47a8d3cad75e5afa73fafb`。不是全 workspace、生产持久化回执或断电验收。

第 225 批（2026-09-09，CTL-03 部分）：

- 新增检查点内存封装，绑定实例、Registry build_hash、plan_hash、历史发布身份和精确日志前缀。创建要求无未决意图/短尾；读取验证整个日志，核对前缀并重编译快照，不以旧完好前缀掩盖后续损坏。
- 新增 5 项测试：新 BootId 重编译、实例/注册表/同序号不同历史拒绝、每个截断/单字节损坏、正确校验但错误前缀/计划，以及后续未决/短尾/损坏分类。
- 当前没有文件写入或原子替换。可信宿主仍须统一协调全来源写者，原子取得日志位置和已发布快照；编码器不能证明两个独立查询属于同一时刻，也不能证明 activation/外部资源已恢复。生产持久化回执与启动恢复继续未完成。
- 验证：Windows 宿主 bin + run_reload 98 项、Linux 99 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `9708ccac2ba3759211415d5a51b6d340e90a07f2e26418dad1122e8ea367438f`。不是全 workspace、真实运行时恢复或断电验收。

第 224 批（2026-09-09，CTL-03 / QA-04 部分）：

- 新增无依赖来源快照 codec：完整保存来源身份/修订/指纹、所有 Node/Value/Property/Origin，浮点按位往返，空来源和属性顺序不丢失。版本校验与有界字节/字符串/来源/节点/值/深度限制同时保护编码与解码。
- 新增 6 项模型测试；规划层已验证“已发布快照→编码→解码→新 BootId 重编译”保持 plan hash。错误不暴露原文，正确校验但非法结构仍拒绝。
- 增加 `snapshot_codec` 模糊测试入口，原始帧与正确校验的任意正文双路径，成功解码必须字节级规范重编码一致。检查点信封、磁盘写入/替换及宿主启动恢复仍未完成。
- 验证：Windows/Linux 模型 57 项、规划 218 项均通过；宿主分别 93/94 项通过。Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 单测镜像 `f9c2f39b49b2695ec5971804a3db1b8cc47a3733e8b1c93e97579bea4369c7d4`；补强真实 65 层 wire 输入拒绝测试后，模型双平台再次通过，最终镜像 `78985070a9c96be336bbacb5acf5add40c009f6d36daf64447a915a2f0b26240`。Linux nightly fuzz 31 秒完成 1,696,483 次执行，退出 0，无崩溃/断言失败；输入上限 4 KiB，不代表长稳、最大输入 RSS 或磁盘恢复验收。

第 223 批（2026-09-09，CTL-03 / RT-04 部分）：

- Committed 固定保存完整编译来源快照，提供可信宿主只读 `source_snapshot()`。深复制及修订向量构建在 generation 写锁外完成，快照与 runtime 一起发布，旧 generation 读者继续持有旧来源。
- 最新失败意图与已发布来源明确分离；NoOp 保留原构建输入，refresh/真实发布建立新输入。新增 4 项测试覆盖拒绝/准备失败/调用者修改、NoOp/refresh、stale 竞争、多来源空贡献及独立新 BootId 重编译。
- 快照可能含敏感配置，不进入 Committed Debug。额外副本随每个被固定 generation 保留；尚未完成内存/性能量化、磁盘序列化、原始文件备份或启动恢复，不宣称 LKG 持久化已完成。
- 验证：Windows/Linux 的 gatel-plan all-targets 各 218 项通过，宿主 bin + run_reload 分别 93/94 项，HTTP reload/source_composition/state_publication/certificate_publication 各 13 项通过。Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过；Linux 镜像 `e5806295b0f536424c4500e67136a3ca0b037abf36cd4ddc672caa4028378071`。不是全 workspace、内存基准或磁盘恢复验收。

第 222 批（2026-09-08，CTL-03 部分）：

- Journal 打开必须显式提供稳定实例身份。第 1 条 frame 写入并同步身份域指纹；外来身份、缺少/重复/不完整绑定、未知版本均拒绝，不自动认领或迁移旧实验日志。
- 身份格式校验在文件操作前执行；绑定校验先于事务回放。不是认证或防恶意文件替换；宿主 CLI 身份配置、日志定位与初始化故障恢复入口尚未接入。
- 新增 5 项测试，含逐字节绑定短尾、不匹配不回放/不改写、非法身份无文件副作用，以及真实子进程 kill 后已应用/拒绝/未知三种事务状态恢复。修改事务夹具顺序号，维持规范结果引用的测试含义。
- 仍未完成全来源 LKG checkpoint、生产持久化回执和启动恢复；测试中的 Applied 是可信夹具写入，不等于已验证实际 Runtime 发布。
- 验证：宿主 bin + run_reload 在 Windows 93 项、Linux 94 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。Linux 镜像 `18be06fe880f9c8261650c220b99f94389b5ca7d8edd6b215224bf4fe52f8533`；未执行全 workspace、断电或真实 HTTP 持久化回执验收。

第 221 批（2026-09-08，CTL-03 部分）：

- 新增 `Journal` 意图/结果协议，持续持锁回放全部 frame；保留最新意图及最后明确确认应用的 admin 贡献，拒绝不覆盖最后确认贡献，未决结果不隐式判成功/失败。
- 严格执行单个未决意图、规范结果引用和不可重复决议；写盘同步成功前不推进可见状态。正确校验但语义非法的日志打开失败，不自动迁移旧裸 KDL 实验记录。
- frame 载荷增加 32 字节信封额度，KDL 仍保持完整 1 MiB 上限；新意图预留结果记录空间，避免逻辑容量上限阻止决议。增加 9 项恢复/短尾/非法转移/失败写入/容量边界测试。
- 尚未接入生产事务。特别是 watcher 也可能发布曾被拒绝的 admin 意图，因此必须记录全来源发布组合，不能把此处最后确认贡献称为完整 Runtime LKG。客户端幂等键、持久化回执、checkpoint 和启动恢复仍待完成。
- 验证：宿主 bin + run_reload 在 Windows 88 项、Linux 89 项通过（all-features/locked）；Windows 严格 Clippy、xtask、无默认功能 check、diff 检查通过。最终 Linux 镜像 `e1d516eec9eec222574ba121b928a63dcf5bc338913ce62cb8d8acfb20fdbb3d`；不是全 workspace 或真实发布/回执崩溃窗口验收。

第 220 批（2026-09-08，CTL-03 部分）：

- 新增内部显式短尾恢复：持锁重新有界读取，核对整个日志的打开时指纹和完整前缀，截断并同步后才恢复追加；打开日志不会自动修复。
- 同长度的完整前缀/未完成正文改写均拒绝；检查失败或截断同步失败污染句柄，不将错误解释为磁盘没有改变。
- 新增逐字节短尾恢复、空前缀恢复、同长度改写拒绝与截断后模拟同步失败测试。仍无生产恢复入口，HTTP 200 仍为内存应用；事务回执、LKG checkpoint 和轮转未完成。
- 验证：Windows journal 14 项、Linux journal 15 项通过；随后宿主 bin + run_reload 在 Windows 79 项、Linux 80 项通过，均使用 all-features/locked。Windows 严格 Clippy、xtask、diff 检查通过。Linux 镜像 `f90eb69c69312449adaa61d0717f792acadcbe8f239188c5e8bfbdd22f71b404`。不是完整 workspace、真实断电或生产事务验收。


### 实施记录（2026-09-07）

| 任务 | 状态 | 已落地与剩余工作 |
|---|---|---|
| RT-02 | 部分完成 | 建连全过程新增共享期限，覆盖 DNS/TCP/TLS/HTTP 握手和 Unix 路径。新增阶段累计预算、阻塞 H2 握手关闭传输、TLS 超时归还连接槽测试。GOAWAY、全面取消和跨平台互操作仍待专项验收。 |
| EXT-01 | 部分完成 | bundle 注册顺序规范化；重复 bundle 身份拒绝；directive scope 纳入构建指纹。公开 API 版本分层、外部夹具和真实裁剪仍未完成。 |

本批验证：Windows 上 `cargo test -p gatel-plan -p gatel-state`、HTTP 的 reload/retry/stability 三套集成测试（21 项）、相关 crate 的 all-targets clippy（warnings 为错误）、`cargo run -q -p xtask` 与 `git diff --check` 通过。新增 6 项回归测试；没有执行 Linux/macOS 或 PHP-FPM 验收。FastCGI 插件仍待实施。

兼容说明：scope 现在参与构建指纹，旧算法与新算法产生的 build/plan hash 不保证相同；KDL 和公开 CLI 语法未变。插件宿主重复注册同名 bundle 现在明确报错，即使其 directive 名称不冲突。

第二批（2026-09-07，RT-02 / RT-04 / EXT-01 部分）：

- drain 成为 pool 级终止状态，尚未出现的 endpoint 也不能重新开放；在途 lease 结束不重新停放 socket。
- 地址允许集合区分未初始化与显式空集合；最后一个 DNS 地址撤回后，在途连接不能回池；验证地址到停放之间持锁，防止 retire 竞态。
- 等待者计数在获得 permit 后结束；降低容量时按实际授予量统计在途 lease，避免低报。
- `order` 拒绝超出 i32 的整数和重复属性；Registry 拒绝非 Observe 插件声明 fail-open。
- 新增 8 项回归测试。Windows 完整 workspace 测试通过；默认忽略的 soak 与一项 doctest 未执行。FastCGI、效果组合编译和跨平台验收仍待实施。

第三批（2026-09-07，EXT-01 / EXT-02 / EXT-04 / QA-02 部分）：

- 新增 `gatel-plugin-api` 纯元数据层；运行时桥接仍在 plan，未宣称稳定 ABI。开发边界与下一阶段架构见 [PLUGINS.md](PLUGINS.md)。
- 编译期稳定拓扑排序支持 `before` 和 `conflicts_with`，拒绝顺序环与冲突；跨作用域约束优先于用户 order，同作用域阶段边界不可打破。约束纳入构建指纹。
- body-limit 声明先于 rate-limit / forward-auth；新增跨作用域、循环、冲突、可选指令缺席测试。完整效果系统仍待实现。
- 新增独立 `plugins/maintenance`，默认不编入宿主；validate/run/capabilities 共用装配入口。xtask 覆盖插件目录，新增包未声明依赖层级时失败。
- 真实宿主进程测试覆盖 GET、HEAD、503/200 双向热切换；关闭 feature 后 capability 消失且配置被拒绝，normal 依赖树不含维护模式包。
- Windows 全功能 workspace 测试、关闭 feature 的宿主集成测试、全目标全 feature clippy、xtask 已执行；默认忽略的长稳测试和 doctest 未执行。尚无 Linux/macOS、外部独立仓库插件、有状态插件生命周期或 CGI/PHP-FPM 的验收证据。

第四批（2026-09-07，EXT-01 / EXT-03 / RT-05 部分）：

- 新增 `gatel-plugin-runtime`，Directive/Config/BuildContext/Bundle、subrequest 与任务工厂移出编译器；维护模式包不再依赖 plan。
- 宿主将 upstream 声明转换为实际调用 allowlist，单后端句柄不接受请求时替换 ID；forward-auth 已迁移。
- 实现 generation 级后台任务作用域：prepare 惰性工厂、commit 激活、owner 去重、64 项预算、重复名称拒绝、幂等激活、最终持有者释放时取消，显式 shutdown 取消并 join。
- 新增 SDK 授权测试、任务生命周期单测及真实编译/事务集成测试；覆盖多路由多 listener 去重、NoOp、prepare 失败保持 LKG、旧 generation 保留任务、释放后取消。
- 剩余：Secret/DNS/Dialer 类型化授权、worker 失败可观测性与恢复、跨 generation worker 复用、CGI 外部进程回收。当前任务取消不是完整 supervisor，也不是 CGI 已完成的证据。
- 本批 Windows 验证：全功能 workspace 测试、全目标全 feature Clippy（warnings 为错误）、xtask、diff whitespace 检查通过；维护模式 normal 依赖树不含 plan/state/http。默认忽略的长稳测试和 doctest、其他操作系统及 CGI 协议后端未执行。

下一批优先级：EXT-03 剩余资源服务及任务失败治理 → CGI-01 非 HTTP terminal 流/取消契约 → CGI-02 独立 FastCGI 包。

第五批（2026-09-07，RT-01 / RT-04 / RT-05 / EXT-03 部分）：

- 修复 shutdown 直接 abort accept loop 而跳过排空的问题。连接由 JoinSet 持有；移除 listener 时保留其排空任务，进程关闭等待活动与退休 listener 回收；超出配置 grace-period 后取消并 join 剩余连接，不再硬编码 30 秒。
- 连接活动计数守卫前移，覆盖 PROXY header 拒绝/失败的提前返回路径。
- 后台任务新增 fallible 工厂与 Pending/Running/Completed/Failed/Panicked/Cancelled 状态快照；工厂与 future panic 被捕获，不污染任务管理锁。状态报告不保存插件错误文本；进程默认 panic hook 仍可能输出 panic 文本，插件不得在其中包含秘密。
- 新增真实 TCP 关闭测试：在途响应完成、未完成请求被关闭、活动计数归零；任务错误/panic/取消状态测试。自动重启、readiness 联动、升级隧道独立任务回收仍需继续验收。

第六批（2026-09-07，RT-04 / RT-05 / QA-03 部分）：

- 移除 proxy upgrade 的 detached spawn；每个连接通过一次性 handoff 接管升级 I/O。HTTP 结束后由同一连接任务运行隧道，持续持有 generation、连接 permit 与活动计数守卫。
- listener drain 的 JoinSet 同时覆盖升级隧道；超出 grace-period 后取消并回收，避免 HTTP 101 后容量与生命周期失管。
- 双端 upgrade 等待使用 response-head 期限；任意一端失败取消另一端等待。
- 新增真实升级测试：热更新后旧 generation 保留、连接上限持续生效、隧道关闭后等待请求恢复、shutdown 期限关闭隧道。另补 handoff 一次性及 HTTP 失败释放未启动资源的测试。
- Windows 全功能 workspace 测试、Clippy 和 xtask 通过；最后一次 handoff 一次性强化另跑单测。Linux/macOS、真实浏览器 WebSocket 与长稳负载仍需后续验收。

第七批（2026-09-07，CGI-01 / CGI-02 部分）：

- 新增纯 `gatel-cgi-protocol`，提供有界环境、增量响应头、正文长度计数、FastCGI record/PARAMS 编码和响应状态机；不依赖运行时或 HTTP 连接池。
- 新增 9 项协议合同测试，包括固定字节向量、逐位置分片、70 KiB 参数跨记录、非法头、长度不一致、stderr 预算和提前 EOF。
- 支持配置和待办写入 [CGI.md](CGI.md)。异步双向交换、请求路径映射、可选 terminal、Unix transport 和真实 PHP-FPM 仍未完成；CGI-01/02 不标为完成。

第八批（2026-09-07，CGI-01 / CGI-02 部分）：

- 新增 `plugins/fastcgi` 的异步交换层，独占 transport、上传/响应并行、有界 record 缓冲、总体及操作等待期限、流式 Output 背压，取消不遗留任务。
- 严格校验 CONTENT_LENGTH 与实际上传；完整早期响应取消剩余上传。响应解析、body 长度、stderr 上限和 END_REQUEST 完成统一走协议层；错误不调用成功 finish。
- 新增 8 项独立双工故障测试，包含 32 字节缓冲下 100 KiB 上传与提前响应、慢上传、下游堵塞、取消、超时和异常结束。
- 尚未注册宿主 feature 或 terminal；路径映射、实际 TCP/Unix dial、HTTP 流式适配与 PHP-FPM 互操作继续实施，不能宣称 FastCGI 已可供用户配置。
- Windows 全功能 workspace 测试、Clippy（全目标、warnings 为错误）、xtask 与 diff whitespace 检查通过；外部 PHP-FPM 与跨平台验证尚未执行。

第九批（2026-09-07，CGI-02 / EXT-04 部分）：

- FastCGI fixed-script terminal 已注册为可选 `fastcgi` feature。新增类型化配置、IP:port/Unix dial、per-directive 容量、有界上传及请求自身驱动的响应流；默认构建不含该包。
- root/script 真实路径 containment、PATH_INFO 单次解码、请求环境与敏感头策略落地；固定 script 模式不会根据 URL 选择磁盘文件。
- 新增真实 TCP responder 集成测试（POST/HEAD、环境与头过滤）、路径安全测试、feature 配置可用性测试；提供可执行配置和 PHP 示例。
- 剩余：真实 PHP-FPM/Unix 验收、未知长度输入策略扩展、动态脚本/远端路径映射、迟到错误与响应流组合矩阵。当前仍不标 CGI-02 全部完成。
- Windows 全功能 workspace 测试、关闭 feature 的 CLI 集成测试、Clippy 和 xtask 已通过；xtask 自身文件超过 600 行建议值（611），尚未超过硬门槛，后续应拆分检查模块。

第十批（2026-09-07，CGI-02 / QA-03 部分）：

- 增加 `remote-root`，支持本地脚本校验与远端 FPM 文件系统路径映射。
- 修复迟到的应用失败：保留最后一个字节直到成功 END_REQUEST；零长度响应在输出头部前等待完成。新增真实 TCP 截断回归，证明正文前缀仍流式交付、失败时不会满足 Content-Length。
- 实际运行隔离的 Linux PHP-FPM 8.4.25 容器，从 Windows Gatel 发起 GET、非空 POST 回显和 HEAD，传输层及 terminal 两项显式互操作测试均通过。
- 实测发现 PHP-FPM 省略空 stdout 记录；接受 END_REQUEST 隐式终止输出，仍验证头、长度、request ID 与状态，增加独立回归，未放宽 bare EOF。
- Windows 全功能 workspace 测试和 Clippy 通过；外部测试默认忽略但本批已显式执行。容器用毕清除，镜像保留缓存；digest 与复现方式见 CGI.md。Unix 与更多平台仍待验收。

基线：当前 `v2@6ca3741`；旧功能基线 `main@eaf9bde`（v0.1.4）；设计参考 `tyreseluo/gatel@0c8bf26`。
本文承接根目录两份早期 `_improve*.md`，以已经实现的 v2 为起点。当前功能证据由 [FEATURES.md](FEATURES.md) 管理，架构由 [ARCHITECTURE.md](ARCHITECTURE.md) 管理；本文不覆盖其实际状态。

第十一批（2026-09-07，CGI-02 / QA-02 部分）：

- FastCGI 新增显式 `buffer-unknown` 和 `max-buffer`：未知长度正文有界缓冲后生成准确 CONTENT_LENGTH；已知长度保持流式上传，默认仍拒绝未知长度。
- 并发 permit 覆盖缓冲到响应结束；缓冲超限/超时/读取错误均在连接后端前拒绝。总期限覆盖缓冲、dial 和交换，连接子期限不再额外延长总预算；无正文响应的后端超时统一返回 504。
- 新增边界单测与真实 TCP chunked 回归：默认 411、超限 413、慢上传 408、失败不访问后端、超时释放唯一额度及分块正文准确转为 STDIN。
- HTTP/2 未知长度、磁盘 spool、Unix socket 和完整 middleware 组合尚待验收；本批不代表 CGI-02 全部完成。
- 本批 Windows 全功能 workspace 测试、全目标全 feature Clippy（`-D warnings`）、xtask 和 diff whitespace 检查通过；默认忽略的 PHP-FPM、soak 与 doctest 未执行。xtask 仍有自身文件 611 行超过建议值 600 的提示，未放宽门禁。

第十二批（2026-09-07，CGI-02 / QA-03 部分）：

- 新增独立真实 HTTP/2 → FastCGI 集成测试，无 Content-Length 的多帧正文准确映射到 CONTENT_LENGTH/STDIN，SERVER_PROTOCOL 使用 HTTP/2.0。
- 验证同连接多 stream 的容量隔离：首个交换占满唯一额度时另一个请求得到 503；响应正文中途取消、响应头返回前取消两种情况均关闭后端 socket 并释放额度，随后同连接新请求成功。
- 测试用 10 秒外层期限约束取消，而后端配置为 60 秒；不能靠等后端超时取得通过。此次补充验收未改变运行时代码。
- 仍待：TLS ALPN / GOAWAY、Unix socket、读入缓冲期间取消以及 body-limit 等 middleware 的错误分类组合。
- Windows 两套 FastCGI 集成测试共 5 项通过（外部 PHP-FPM 1 项保持忽略），gatel-http 全目标 Clippy（`-D warnings`）、xtask 与 diff whitespace 检查通过；本批未重跑整个 workspace。

第十三批（2026-09-07，EXT-03 / CGI-02 / QA-02 部分）：

- SDK 新增 `BodyTooLarge` 类型错误，宿主全局正文限制与标准 body-limit 使用同一分类，避免插件依靠字符串猜测客户端错误。
- FastCGI 未知长度缓冲保留该分类，全局／路由限制触发时返回 413 而非 400，不访问后端；通过真实 TCP 两种组合验证。
- 错误查找显式处理 Salvo 与 I/O 包装，保留其他标准 source 链并限深 32；单测覆盖嵌套包装、同文本普通错误不可冒充和循环 source 链。
- 其他 terminal 的错误分类、缓冲期间取消及 Unix transport 验收仍待完成。
- Windows 全功能 workspace 测试、全目标全 feature Clippy（`-D warnings`）、xtask 和 diff whitespace 检查通过；默认忽略的外部 PHP-FPM、soak 与 doctest 本批未执行。

第十四批（2026-09-07，RT-04 / QA-02 部分）：

- 全局请求体限制与标准 body-limit 在超限、输入错误和 EOF 后进入终止状态并立即释放输入；即使消费者继续轮询，也不再消费或交付后续帧。
- 结束状态的 size_hint 为精确零；正文计数改为先检查剩余额度再相加，避免 u64 边界溢出。
- 新增 4 项回归，分别验证两层实现的错误后输入 Drop、重复轮询、结束提示和 u64 边界；不将正文错误后的 EOF 解释为本次请求成功，消费者仍必须处理此前的错误。
- Windows 全功能 workspace 测试、严格全目标 Clippy、xtask 和 diff whitespace 检查通过；外部 PHP-FPM、soak 与默认忽略的 doctest 本批未执行。

第十五批（2026-09-07，CGI-01 / CGI-03 部分）：

- 新增 SCGI 请求环境 netstring 编码：首字段 CONTENT_LENGTH、准确正文长度校验、唯一 SCGI=1、稳定字段顺序与有界字节预算。
- 依据原始协议的固定 70 字节向量验证，另测无正文、非数字／溢出／长度不一致、标记冲突、自动字段与 UTF-8 字节计数；新增 4 项回归。
- 协议 crate 全部 14 项测试与严格 Clippy 通过。此批仅完成纯协议层，SCGI 异步交换、可选插件注册、HTTP 映射及独立后端验收仍未完成，不能宣称 SCGI 已可用于部署。

第十六批（2026-09-07，CGI-01 / CGI-03 部分）：

- 新增独立 gatel-scgi 包的异步交换层，单次连接所有权、并行上传／下载、半关闭、整体及 I/O 期限、有界 CGI 头解析、EOF／正文长度验证与最后一字节延迟交付。
- 独立测试后端在 32 字节 duplex 缓冲下验证 100 KB 双向传输，另测响应截断／超长／HEAD、超时释放连接、提前响应取消输入；4 项测试与严格 Clippy 通过。
- 尚未注册宿主 scgi 指令／feature；真实 TCP、外部取消及完整流式 HTTP 适配仍待实施。没有因加入 workspace 包而宣称部署能力完成。

第十七批（2026-09-07，CGI-03 / QA-03 部分）：

- 独立真实 TCP 后端检查 SCGI 请求固定字节和上传半关闭，再回传 CGI 响应；双工内存测试之外已有实际 socket 证据。
- 补充外部取消释放输入／transport，以及输出回调停滞按 idle 超时而不调用 finish 的回归。
- gatel-scgi 全部 7 项测试与严格 Clippy 通过；宿主 feature／terminal 和完整 HTTP 映射尚未完成，真实应用服务器未验收。本批未改运行时实现。

第十八批（2026-09-07，CGI-01 / CGI-03 / EXT-04 部分）：

- 新增 gatel-cgi-http 专用共享适配层，将 FastCGI 原有 HTTP 映射、缓冲、容量、dial、期限、响应流和取消逻辑迁入；两个插件各自保留协议交换，通过 Protocol/ExchangeRequest/Sink 接入。
- 新增默认关闭的 scgi feature、统一 registry 装配与示例；缺失 feature 时 validate 拒绝指令，capabilities 不暴露它。两个 CGI feature 均关闭时 normal 依赖树不含两个插件及共享适配包。
- 新增真实 HTTP → TCP SCGI 测试：chunked 转准确 netstring/正文、PATH_INFO、认证头默认过滤和 HEAD；FastCGI 原有 H1/H2、超限与取消回归全部通过。
- Windows 全功能 workspace 测试、关闭 feature 的宿主测试、严格全目标 Clippy、xtask 通过。默认忽略的 PHP-FPM、soak 与 doctest 本批未跑；Unix、SCGI 真应用、专项 H2/reload 仍待验证。
- 原 FastCGI input/mapping/terminal 文件迁移到共享包，不保留复制实现；错误正文改为 CGI 通用消息。共享适配器不承担子进程树生命周期，子进程 CGI 尚未实现。

第十九批（2026-09-07，CGI-02 / CGI-03 / QA-02 部分）：

- remote-root 的纯语法校验移到 decode，保留 prepare 防御性校验；错误不再依赖本地文件存在性才被发现。
- script 拒绝控制字符、空片段和显式点片段，避免 Path::components 规范化掩盖原始配置；server-name 拒绝控制字符。
- 新增 2 项 decode 回归并扩展路径矩阵；共享适配层 5 项测试、FastCGI/SCGI HTTP 回归 7 项及严格 Clippy 通过。外部 PHP-FPM 本批未执行，未重跑整个 workspace。

第二十批（2026-09-07，CGI-03 / RT-01 / QA-02 部分）：

- 新增 SCGI 真实 TCP 热更新回归：响应停在最后一字节之前，缺失脚本导致 PrepareFailed，generation 不变且新连接仍得到旧配置；随后成功更新，新连接得到新配置。
- 在途 SCGI 响应持有旧 generation，后端 EOF 后完成最后一字节；连接关闭后 Weak 引用确认旧 generation 释放。
- 此项验证 H1 独立连接、失败候选和成功更新，不代替 H2 复用、Unix 或真实应用服务器验收。
- SCGI 两项 HTTP 集成测试、gatel-http 严格全目标 Clippy、xtask 和 diff whitespace 检查通过；本批未修改运行时代码、未重跑整个 workspace。

第二十一批（2026-09-07，EXT-03 / CGI-01 / QA-02 部分）：

- 共享 CGI Sink 强制 head/body/finish 状态机，协议 future 返回成功还必须具有完成标记；违反顺序或取消未完成回调后不能忽略错误恢复为成功。
- 新增 3 项输出合同测试：漏 finish、重复头／完成／提前或迟到正文、阻塞回调被取消；共享包共 8 项测试通过。
- FastCGI/SCGI HTTP 集成共 8 项、全 workspace 严格 Clippy 通过。协议层仍负责头／正文长度及 EOF/END_REQUEST 的真实性，状态机不代替其校验；本批未重跑全 workspace 测试。

第二十二批（2026-09-07，CGI-01 / EXT-03 / QA-02 部分）：

- CGI Sink 将任意大小正文回调拆为最多 8 KiB 的消息，先取得通道空位再复制；适配器不再额外分配整个插件输出切片。
- 新增完整大块输出顺序／分块测试，以及发送一块后取消的队列上限／失败状态测试。该界限不包括插件原始切片与 HTTP 栈缓冲。
- 共享适配层共 10 项测试、FastCGI/SCGI HTTP 集成共 8 项通过；外部 PHP-FPM 本批保持忽略，未重跑全 workspace 测试。

第二十三批（2026-09-07，EXT-04 / QA-01 部分）：

- xtask 依赖许可从主检查文件拆为独立 layers 模块，保留原有规则及行数门槛；消除自身超出建议文件长度的提示。
- 新增规则自检：重复层、重复许可、自依赖和内部循环许可拒绝，避免 BTreeMap 收集时静默覆盖重复项，或未来新增插件后引入跨层循环。
- 4 项规则测试及完整 xtask 检查通过；共享 CGI 测试模块声明移至文件末尾，避免生产代码行数统计提前截断。运行时行为未改。

第二十四批（2026-09-07，EXT-04 / QA-01 部分）：

- 架构依赖检查使用离线 cargo metadata --no-deps 解析声明，覆盖平台条件、optional、重命名包、workspace 继承和 build-dependencies；只豁免 dev-dependencies，不依赖当前启用的 feature 或平台过滤。
- metadata 失败／格式错误不再当成空依赖成功；目录中的清单若不是被检查的 workspace member 会报错。xtask 自身为构建工具，不是网关层。
- 新增 3 项解析测试，其中真实临时 workspace 声明 Unix-only、optional、子表形式的别名依赖，在 Windows 仍识别真实包名；fixture 用毕清理。
- xtask 全部 7 项测试、严格 Clippy、完整架构门禁和 diff whitespace 检查通过。此次只修改开发门禁，未重跑网关全测试。

第二十五批（2026-09-07，CGI-01 / RT-04 / QA-02 部分）：

- 增加共享 CGI 适配器的缓冲中途取消回归：先读入部分未知长度正文，随后等待输入；唯一并发额度此时占用，竞争请求得到 503。
- 丢弃执行 future 后立即观测输入 Drop 和额度恢复，再次请求可重新占用并释放；真实监听 socket 确认无后端连接。此项是直接适配器取消测试，不冒充 HTTP/2 RST_STREAM 端到端验收。
- 本批不改运行时实现，继续保留整个完善目标。
- 共享适配层全部 11 项测试、xtask 和 diff whitespace 检查通过；未重跑网关全测试或外部协议服务器。

第二十六批（2026-09-07，CGI-03 / QA-02 部分）：

- 新增 SCGI 上传短读／超长回归，逐字节核对实际请求：超长部分不进入后端，错误关闭 transport 且不调用 finish。
- 新增声明长度超预算的零发送验证。测试针对后端未完成响应时的上传错误，不改变完整提前响应取消上传的既有策略。
- gatel-scgi 全部 9 项测试、严格 Clippy 通过；本批只补验收，未修改运行时代码或重跑整个 workspace。

第二十七批（2026-09-07，CGI-02 / CGI-03 部分）：

- 共享 CGI 配置新增 idle-timeout，默认 15 秒、范围 1～3600，参与配置指纹并传入两个协议交换层；总 timeout 仍是最终上限。
- 配置回归覆盖非法边界与指纹变化；真实 SCGI 后端输出零正文响应头后保持连接不结束，验证 idle-timeout=1 在 timeout=30 之前返回 504。
- 共享层 12 项测试、SCGI HTTP 集成 3 项、全 workspace check 与严格 Clippy 通过。首次响应和预缓冲的特殊期限语义已记录，未宣称 packet-level 活动计时。

第二十八批（2026-09-07，EXT-03 / CGI-01 部分）：

- 协议工厂延迟到受保护 future 内调用，工厂及轮询 unwind panic 统一转为 CGI 交换失败；不转发 panic 文本，也不能因先调用 finish 而恢复成功。
- 新增交换 panic 的资源 Drop／完成状态测试，并扩展适配器测试验证工厂 panic 返回 502、归还额度和关闭已连接 socket。
- panic=abort、默认 hook 输出和进程内不受信任代码不在保证范围，已记录插件边界；整体目标未完成。

第二十九批（2026-09-07，EXT-04 / QA-01 部分）：

- CI 的 Linux/macOS/Windows 测试同时覆盖默认与全 feature；lint 和 MSRV 检查启用全 feature，防止可选宿主注册入口漏检。
- 新增独立插件矩阵 maintenance/fastcgi/scgi，执行宿主 capabilities/validate 测试与 normal 依赖树隔离检查；声明检查使用 locked 构建。
- 本机 Windows 完整 workspace 全 feature 测试通过，三个单 feature 宿主测试共 10 项通过，三个生产依赖树隔离检查通过。
- 远端 CI 未推送／执行，不能把 workflow 修改当成 Linux/macOS/MSRV 实测；默认忽略的 PHP-FPM、soak 与 doctest 本批未执行。

第三十批（2026-09-07，QA-04 / CGI-01 部分）：

- 新增 `cgi_protocol` fuzz 目标并接入 Linux CI 矩阵；覆盖 CGI 头分片一致性、FastCGI framing/响应状态机、环境预算与 SCGI/FastCGI 编码往返。独立测试解码器核对每个变量、PARAMS 结束记录和请求 ID。
- 与 stable 测试共用不变量代码，运行固定向量、1,024 组确定性生成输入及 127/128 字节、64 KiB 跨记录、128 KiB 输入边界，便于无 fuzz 工具链的开发环境回归。
- Windows `cargo test -p gatel-cgi-protocol --locked` 通过 15 项测试。实际 coverage-guided fuzz 尚未运行成功：默认构建缺少 ASan MSVC runtime，关闭 sanitizer 后仍缺少 sancov 链接符号；不能将 stable 回归或 CI 配置视作 sanitizer 验收通过。Linux fuzz、长时间 corpus 演化及跨平台验证仍待执行。

第三十一批（2026-09-07，EXT-02 部分）：

- 元数据层新增类型化请求侧 Effect，SDK 重导出；限制器安装先于正文读取、上游调用、额度扣减，不再靠标准插件名称识别。标准 body-limit、forward-auth、rate-limit 已迁移。
- 效果边与现有 before/phase 边共同进行拓扑编译；覆盖跨作用域、极端 order、显式反向约束、阶段冲突及两个互相无法保护读取的复合插件。集合规范化参与构建身份，顺序/重复项不影响 hash，实际行为变化影响 hash。
- 新增 3 项效果组合/指纹测试；Windows 编译器与标准包相关 382 项测试通过。该批不是完整 Effects 验收：响应侧、缓存/认证/CORS 组合、terminal 冲突以及解释性计划输出仍需实现。
- 最终 Windows 全功能 workspace 测试、全 feature/all-targets 严格 Clippy、xtask 与 diff whitespace 检查通过；默认忽略的 PHP-FPM、soak 和 doctest 未运行，Linux/macOS 未在本批执行。

第三十二批（2026-09-07，EXT-02 部分）：

- 新增统一 `order_route` 编译入口，规范化与计划生成同时校验 terminal 边界；此前仅检查 middleware 链，terminal 声明的冲突可能未被执行。
- 检查 middleware/terminal 双向 conflicts、内核 proxy 冲突、terminal 显式 before 及效果产生的反向顺序；拒绝需要 terminal 提前执行的组合，不通过重排改变终结语义。插件 terminal 诊断关联双方来源。
- 新增外部插件集成测试覆盖 site/route 两种作用域、双向冲突、反向效果、合法前置与缺席约束，验证通过规范化进入真实 compile 路径。响应侧效果与完整安全组合矩阵仍待实施。
- 本批 Windows 编译器与标准插件 384 项测试、gatel-plan all-targets 严格 Clippy、xtask 和 diff whitespace 检查通过；没有在本批执行全 workspace 或跨平台测试。

第三十三批（2026-09-07，EXT-02 / WEB-03 安全修复）：

- 真实 HTTP 测试复现 site 缓存命中绕过 route 限流：max=2 时第三次仍为 200。原测试只覆盖同作用域，未发现此缺口。
- 新增 CacheLookup 效果，所有 Guard 跨作用域先于缓存查找；标准缓存迁移，外部插件同样受约束。显式 order 无法绕过，反向 before 拒绝编译。
- 限流成功响应头改为下游返回后写入，防止缓存保存或重放旧剩余额度。新增真实测试覆盖首次存储、命中仍扣额度、额度头更新、第三次 429、后端仅访问一次。
- Windows 缓存/指令/外部插件 34 项集成测试通过。响应压缩与缓存 variant、CORS/认证全面组合验收仍待继续，不将本修复视为完整响应效果系统。
- 相关三个 crate 的 all-targets 严格 Clippy、xtask 和 diff whitespace 检查通过；本批未执行完整 workspace、跨平台或长稳测试。

第三十四批（2026-09-07，WEB-02 / WEB-03 安全修复）：

- 真实 HTTP 测试复现 encode 覆盖后端 Vary：不同 Accept-Language 请求只访问后端一次，错误复用缓存。同步检查发现 CORS 与缓存只读取第一行 Vary。
- 新增共享 Vary 解析/追加模块，保留多行全部维度、大小写去重，保留 wildcard；无效维度或不可解析值按 wildcard 禁止共享缓存。encode/CORS 不再覆盖原维度，缓存 freshness 与 variant 签名共用完整解析。
- 覆盖语言隔离、重复请求命中、wildcard 不存储及多行变体；更新 encode/CORS/cache 元数据版本。Windows 标准包 243 项测试、缓存 12 项集成测试、相关包 all-targets 严格 Clippy、xtask 与 whitespace 检查通过。
- 本批未执行全 workspace、跨平台或真实浏览器 CORS 验收；完整响应侧效果与压缩实体标签/Range 联合契约仍需继续。

第三十五批（2026-09-07，WEB-02 表示元数据修复）：

- 动态 encode 现在尊重请求与响应多行 Cache-Control 中的 no-transform，并跳过 206/Content-Range，避免压缩部分响应而留下原字节范围。
- 真正压缩时弱化强 ETag，保留已有弱 ETag，移除原 Content-Length、Accept-Ranges、Content-MD5、Digest、Content-Digest、Repr-Digest；不压缩时保留原正文及实体元数据。encode 元数据版本更新为 3。
- 新增元数据单测与真实 HTTP 四场景回归：强 ETag/摘要、响应 no-transform、请求 no-transform、Content-Range；Windows 标准包 244 项测试、缓存/指令 31 项集成测试、严格 all-targets Clippy、xtask 通过。
- 依据 [RFC 9530](https://www.rfc-editor.org/rfc/rfc9530.html) 的内容编码与完整性字段关系处理摘要。HEAD/304 与内容协商的完整联合验收、trailer 摘要重计算仍未完成；本批未运行全 workspace 或跨平台测试。

第三十六批（2026-09-07，WEB-03 策略解析修复）：

- 缓存改为解析全部 Cache-Control 字段行；识别带参数的 private/no-cache，支持引号中的逗号与转义，不再把第一行视作完整策略。
- 重复指令、非法/溢出 freshness、无效或未闭合语法按不缓存处理，不落回 default TTL；请求的多行 no-cache/no-store、带空白和引号的 max-age=0 正确绕过缓存。cache 元数据版本更新为 3。
- 新增解析单测与真实 HTTP 请求/响应策略矩阵。Windows 标准包 245 项测试、缓存 15 项集成测试、相关 all-targets 严格 Clippy、xtask、whitespace 检查通过。
- 该缓存仍不提供完整重验证或所有请求 freshness 指令语义；本批未执行全 workspace、跨平台或长稳验收。

第三十七批（2026-09-07，WEB-03 请求新鲜度）：

- 修复 Store 过期判断忽略上游 Age 的问题，使用上游年龄加本地驻留时间的饱和累加，同时供 Age 响应头与新鲜度判断使用。
- 请求 max-age/min-fresh 按累计年龄和剩余 TTL 校验，不能满足时回源；min-fresh 非法数值不允许缓存复用。Store 在计数和更新 LRU 前检查约束，被拒绝的候选记为 miss，不误报 hit，也不删除仍适用于其他请求的条目。
- 新增 Age 到期/极值、候选拒绝计数单测及四场景真实 HTTP 矩阵；Windows 标准包 246 项测试、缓存 16 项集成测试通过。cache 元数据版本更新为 4。
- 尚未完成基于 Date/网络延迟的完整 corrected initial age、only-if-cached、条件重验证或 stale 策略；不得把本批理解为完整 RFC 缓存实现。

第三十八批（2026-09-07，WEB-03 cache-only）：

- 支持有效 Cache-Control 中的 only-if-cached：仅有满足约束的缓存响应才返回内容，否则生成 no-store 的 504，不调用下游。与 no-cache/no-store/max-age=0/min-fresh 等有效组合共同执行；HEAD 错误响应无正文。
- 真实 HTTP 验证冷缓存零回源、暖缓存命中、约束不满足零额外回源；跨作用域限流测试改用 only-if-cached 验证其不能绕过已耗尽额度，仍返回 429。cache 元数据版本更新为 5。
- Windows 标准包 246 项测试、缓存 17 项集成测试、相关 all-targets 严格 Clippy、xtask、whitespace 检查通过。本批未运行全 workspace/跨平台；完整 corrected age 与条件重验证仍待实施。

第三十九批（2026-09-07，WEB-03 年龄校正与日期安全）：

- 初始年龄按 max(基于 Date 的表观年龄, Age + 下游等待时间) 计算，Duration 保留亚秒精度；resident 起点前移到下游返回，不再遗漏正文收集时间。非法/重复 Date 或 Age 不入库，Age 数值溢出保守饱和。
- 删除逐年遍历的手写 HTTP-date 解析器，避免巨大年份导致无界 CPU 循环；直接依赖已在依赖树中的 httpdate 1.0.3，支持旧 HTTP 日期格式并验证非法日期。架构白名单登记纯解析依赖；cache 版本更新为 6。
- 新增日期/延迟/时钟偏差/重复字段单测，真实 HTTP 覆盖旧 Date 与非法 Age。Windows 标准包 247 项测试、缓存 17 项集成测试、相关严格 Clippy、xtask 通过。
- 依据 [RFC 9111 §4.2.3](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.2.3)。时间采样位于缓存中间件边界，嵌套中间件等待可能保守增加年龄；完整协议级时间戳和慢网络压力验收仍需后续验证。本批未运行全 workspace/跨平台。

第四十批（2026-09-07，WEB-03 新鲜缓存条件请求）：

- 新鲜 200 缓存支持 If-None-Match 弱比较、标签列表（含 opaque tag 内逗号）、wildcard 和 If-Modified-Since；ETag 条件优先于日期条件。命中条件返回无正文 304，保留验证元数据、移除 Content-Length/Content-Range，不改变存储的完整响应。
- If-Match、If-Unmodified-Since、Range/If-Range 和无法解析的 ETag 列表不使用本地缓存，交下游处理；与 only-if-cached 组合则继续遵守不回源要求。cache 版本更新为 7。
- 新增解析单测与真实 HTTP 条件请求矩阵，验证 304/200、条件优先级、缓存正文未被304覆盖以及需要回源的条件。Windows 标准包 248 项测试、缓存18项集成测试、相关严格 Clippy、xtask 与 whitespace 检查通过。
- 语义依据 [RFC 9110 §13.1.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1.2)。此批实现客户端对新鲜缓存的验证，不等同于过期缓存向源站发条件请求并合并 304；后者及全 workspace/跨平台验收仍待实施。

第四十一批（2026-09-07，WEB-03 / WEB-07 存储原子性）：

- Vary 元数据与条目合并到同一 Contents 锁；请求签名在该锁内基于当前维度生成，消除外部先读 Vary 再查条目的竞态。维度变化原子清除旧副本并更新容量计数，淘汰与过期同时清除元数据。
- variant 签名覆盖所有同名请求头的原始字节、顺序与字段数量，使用长度分隔哈希；不再只读取第一行，也不把非文本值当作缺席。无维度仍使用空签名。cache 版本更新为 8。
- 新增维度切换/计数单测、并发读写检查和真实 HTTP 重复头隔离测试。Windows 标准包 250 项、缓存 19 项测试，全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。
- 这是后续重验证的存储基础，不代表 304 合并、失效写入竞争或请求合并已经完成。
- 最终 Windows 全功能 workspace 测试通过；默认忽略的 PHP-FPM/soak/doctest 未运行，其他操作系统未在本批执行。独立 fuzz workspace 的锁文件同步了标准包的 httpdate 依赖，但未重新执行 fuzz。

第四十二批（2026-09-07，WEB-03 / WEB-07 写后失效）：

- 缓存中间件看到非 GET/HEAD/OPTIONS/TRACE 方法成功或重定向响应后，清除同一 host/request-target 的全部方法和 variant；条目与 Vary 元数据、容量计数在同一锁内更新。
- 源站填充前获取存储 revision，入库在同锁校验。失效即使发生在尚无缓存条目时也推进 revision，阻止先前在途 GET 重新填入旧响应。使用实例级 revision，避免无界 URI tombstone；可能保守拒绝其他 URI 的在途填充，已缓存的无关 URI 保留。revision 饱和后禁止继续填充，不绕回。
- 新增单测覆盖 GET/HEAD 副本、未存储 URI 失效与旧 revision；真实 HTTP 验证写后回源，另有 oneshot 控制顺序的 TCP 测试证明 GET 开始→POST 完成→旧 GET 返回时旧正文不入库，后续新正文可缓存。cache 版本更新为 9。
- Windows 标准包 251 项、缓存 20 项和独立竞态 1 项测试通过。该失效仅覆盖经过同一缓存实例的有效请求 URI；跨实例通知、Location/Content-Location 关联 URI 和源站外部变更通知尚未实现，不宣称完整分布式一致性。

第四十三批（2026-09-07，WEB-03 同源关联失效）：

- 成功写请求同时处理 Location/Content-Location 的同源目标失效，解析绝对/相对 URI、查询与片段；拒绝跨协议/主机/端口、凭据、异常空白及重复关联字段。使用 url 2.5.8 纯解析依赖，并登记架构边界。
- 缓存键补充协议维度，依据监听器提供的 gatel.tls，而非客户端伪造头；HTTP/HTTPS 的响应及失效隔离。cache 版本更新为 10。
- 新增 URI 解析矩阵、协议隔离存储单测，真实 HTTP 证明相对 Location 使目标回源，跨源 Content-Location 不清除其他主机缓存。Windows 标准包 253 项、缓存 21 项及竞态 1 项测试、xtask 通过。
- 此机制仍限定同一缓存实例；独立实例间的一致性通知未完成。协议维度已有单测但真实双 HTTP/TLS listener 组合尚待验收；本批未运行全 workspace/跨平台。

第四十四批（2026-09-07，WEB-03 / QA-04 容量完整性）：

- 修复缓存容量只计算正文/响应头、遗漏请求键与 variant 元数据的问题。每个条目收取非零固定成本，并包含重复 map key、协议/主机/路径、完整签名、Vary 名称及头表开销；空响应也不能无限积累。
- Held 保存入库时确定的 charge，替换、维度切换、过期、失效与淘汰扣减同一数值；元数据超过整个预算的候选直接拒绝，不清空现有缓存。
- 新增 1,000 个空响应持续写入的有界测试及长路径/签名/Vary 拒绝测试；原容量与 LRU 测试按新预算语义更新，仍验证精确替换、淘汰对象与清理。cache 版本更新为 11。
- Windows 标准包 254 项、缓存 21 项及竞态 1 项测试、严格 Clippy、xtask、whitespace 检查通过。max-size 是保守逻辑存储成本，不是进程 RSS 硬上限；在途响应和分配器实际开销另计。同样配置可能比之前更早淘汰，这是修复后的预期变化。

第四十五批（2026-09-07，WEB-03 / RT-05 缓存策略热更新）：

- 真实 HTTP 复现 max-size 从 4096 降至 128 后仍使用旧预算与旧缓存；根因是空状态兼容指纹令所有配置复用不可变预算 Store，原注释所称下次写入应用新预算并未实现。
- 状态兼容指纹纳入 max-size/max-entry/default-ttl。策略改变为新代创建新 Store，不在 prepare 期间修改活动 Store；相同策略继续复用热缓存。cache 版本更新为 12。
- 真实测试覆盖无关路由更新的 Reloaded（不只是 NoOp）保留缓存、缩容生效、重新扩容可缓存，以及候选中另一 file-server prepare 失败时原缓存不变。Windows 标准包 254 项、缓存 22 项与竞态 1 项测试、严格 Clippy/xtask/whitespace 检查通过。
- 明确行为变化：缓存策略调整导致冷缓存，新老代存储在排空期间可能同时存在，进程占用不等于新代单个 max-size；跨代共享存储的在线缩容尚未实现。本批未运行全 workspace/跨平台。

第四十六批（2026-09-07，EXT-03 / WEB-03 执行行为命名空间）：

- SDK 新增 ExecutionIdentity，宿主在分派前注入；路由身份包含路由 ID/匹配器、完整执行链、terminal、全局配置及实际声明引用的 upstream 配置。无关路由不改变该路由指纹，fallback 使用整份 plan 指纹保守隔离。
- 缓存键纳入该命名空间并计入容量；同名 upstream 切换目标或执行链变化不再复用旧响应。缺失宿主上下文时不缓存，only-if-cached 返回 504。失效仍跨同 Store 的行为命名空间清除同源 URI，旧代条目由预算/TTL 回收。cache 版本更新为 13。
- 真实 HTTP 验证同名 upstream 从 old 切到 new 后回源并缓存新响应；之前无关路由 Reloaded 保留缓存的测试仍通过。Windows 标准包 254 项、缓存 23 项及竞态 1 项测试通过。
- 这是声明配置的行为标识，不是外部数据版本。文件原地修改、DNS 后端轮转或插件未声明的依赖不会自动触发指纹变化；完整资源版本与主动失效仍待完善。

第四十七批（2026-09-07，WEB-07 源站重验证首个执行闭环）：

- 对无客户端前置条件的 GET，从 Store 原子取走匹配的过期候选及失效 revision；未编码 200 且具有合法 ETag 时注入 If-None-Match，调用下游后移除内部条件头。取走后立即返还存储预算，候选由请求 future 持有，取消不遗留后台任务。
- 304 校验 ETag、长度与表示编码边界，合并新响应头、清除旧 Age/Date 和传输字段，复用正文构造完整 200，再按新策略/年龄/容量校验入库。304 验证失败返回 502；新 200 走正常替换，不启用 stale-on-error。
- 重验证提交同时检查失效 revision 和并发替换：其他请求已经写入的新副本不被迟到候选覆盖；Cache-Status 报告源站 304。cache 版本更新为 14。
- 新增独立 TCP 后端验证条件请求、304 正文复用及后续命中、200 替换、错误 ETag；增加并发替换/失效的存储测试。Windows 标准包 255 项、缓存/竞态/重验证 25 项测试，全 feature/all-targets 严格 Clippy、xtask 和 whitespace 检查通过。
- 当前边界：不对编码表示、HEAD、Last-Modified-only 或客户端已有条件执行该优化，仍完整回源；缺失或不匹配 ETag 的 304 不自动重试。请求合并、这些表示的重验证与跨平台/完整 workspace 验收继续推进，WEB-07 不标完成。

第四十八批（2026-09-07，WEB-07 日期验证器与类型化重验证）：

- 重验证选择变为类型化 Validator：合法 ETag 优先，缺失时使用单一合法 Last-Modified；无效或重复 ETag 不降级到日期。条件头注入、恢复和 304 校验使用同一个已选对象，cache 版本更新为 15。
- 日期条件下允许 304 省略 Last-Modified；若返回，保守要求同一时刻（接受等价旧式 HTTP 日期）。拒绝非法、重复或不一致日期与非法新 ETag；合法新 ETag 可更新缓存元数据。重复 Content-Length 在合并前拒绝。
- 增加 4 项验证器单测及独立 TCP 日期重验证测试（8 个场景），覆盖真实条件头、完整正文重建、后续命中、错误响应不保留旧副本。功能表同步当前缓存与 CGI 实施状态。
- Windows 完整 workspace/all-features/locked 回归通过（标准包 259 项、缓存/竞态/重验证 26 项），全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过；默认忽略的测试及 Linux/macOS 验收未执行。编码表示、HEAD、客户端已有条件、请求合并与 stale-on-error 仍待实现，WEB-07 未完成。

第四十九批（2026-09-07，WEB-03 / WEB-07 编码协商与变体隔离）：

- 检查编码重验证前置条件时发现动态压缩与预压缩选择规则不一致：前者通配符可覆盖显式拒绝，后者前缀匹配可接受 gzip-extra，且两者只读第一行字段。新增共享 Accept-Encoding 选择模块，读取全部字段，以整数解析合法 q 值，客户端质量优先、服务端顺序打破平局，显式项优先于通配符；重复项取较低质量，非法输入保守禁用编码选择。
- 动态压缩与静态预压缩共用选择器；显式更高 identity 偏好返回原文。预压缩的 identity 副本也合并 Vary: accept-encoding，防止先缓存原文后遮蔽其他表示。encode/file-server 元数据版本分别更新为 4/2。
- 新增 4 项选择器单测及真实 HTTP/缓存组合测试：两种编码路径各 9 个场景、每种请求重复验证命中；gzip 夹具由真实编码器生成并逐字节比较。测试明确断言配置 Reloaded，避免无效配置留在 LKG 后误测旧行为。
- Windows 标准包 263 项、HTTP directives 19 项测试通过；全 feature/all-targets 严格 Clippy 与 xtask 通过。本批未重跑完整 workspace 或跨平台验收。
- 编码响应重验证仍待表示来源追踪：必须区分源站编码与网关变换后正文，避免把源站 304 的强 ETag、长度或摘要直接套到变换后的字节。所有表示均拒绝时的统一 406 策略、HEAD 表示元数据、请求合并与 stale-on-error 继续保留为待办。

第五十批（2026-09-07，WEB-03 静态文件辅助路径与表示元数据）：

- 目录索引和预压缩副本通过统一 contained_file 解析真实路径、检查根目录包含关系并限定普通文件，关闭辅助路径绕过原文件 containment 的缺口；拼接压缩后缀保留原生 OsString，不再经 display 做有损路径转换。允许根目录内符号链接，拒绝解析到根外的辅助文件。
- 预压缩表示先选择再生成 ETag/Last-Modified 与判断 304。ETag 使用所选文件元数据并加入编码标识，避免 identity 条件错误命中压缩副本；304 保留 Vary 和编码元数据。HEAD 返回所选表示长度，不读取正文。file-server 版本更新为 3。
- 新增跨平台目录外索引单测、Unix 辅助符号链接单测，以及 2 项真实 HTTP 测试，覆盖根外索引拒绝、原文/编码验证器隔离、304 元数据、HEAD 长度与仅更新压缩副本后的新响应。
- Windows 标准包 264 项、HTTP 单元 57 项、directives 19 项、file_variants 2 项测试通过；全 feature/all-targets 严格 Clippy 与 xtask 通过。Unix 符号链接测试、其他平台与完整 workspace 本批未执行。
- 此修复仍是 canonicalize 后的路径级隔离，不宣称能够抵御具有根目录写权限的对手在检查与打开之间替换文件/目录；基于打开句柄的原子路径约束仍待实现。动态压缩 HEAD 元数据、完整文件条件请求、编码响应重验证的来源追踪仍待完善。

第五十一批（2026-09-07，WEB-03 条件请求与共享解析）：

- 新增共享 preconditions 模块，静态文件按 [RFC 9110 §13.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.2.2) 顺序处理 If-Match / If-Unmodified-Since / If-None-Match / If-Modified-Since，再进入 Range 选择。标签条件优先于对应日期条件；If-Match 强比较，If-None-Match 弱比较；无效或重复日期忽略，无效标签语法返回 400。
- 缓存和静态文件共用整字段 ETag 列表解析，保留标签内逗号，完整读取多行；拒绝跨行 wildcard 与标签混合，不因前面的匹配项而跳过后续非法内容。缓存不擅自判定原站前置条件，仍将 If-Match/If-Unmodified-Since 转发。file-server/cache 版本更新为 4/16。
- 400/412 使用明确纯文本错误正文、清除原表示的编码和验证器并设置 no-store；HEAD 不发送错误正文，404 仍优先于前置条件。
- Windows 标准包 265 项、directives 19 项、缓存/竞态/重验证 26 项、file_variants 2 项通过；新增 file_conditions 1 项覆盖缓存关闭/开启各 18 个 HTTP 条件组合及缺失资源。共享解析测试替代原缓存内重复解析测试。完整 workspace、跨平台、动态压缩条件响应、日期 If-Range 与原子文件读取仍待专项验证或实现。

第五十二批（2026-09-07，WEB-03 已打开文件表示与精确长度读取）：

- 新增 OpenedFile 所有权对象：打开后通过同一文件句柄获取元数据，并贯穿验证器、条件请求、HEAD 与范围正文发送；ranges::deliver 改为消费该对象，不再接受独立路径和外部 size。路径被替换后不会为正文重新打开新文件，file-server 版本更新为 5。
- 新增 ExactReader：限制最多发送选定长度，并将提前 EOF 显式报错。内存与流式路径一致，避免截短内容被误认为成功完成并进入缓存；对外调用传入的非法范围在计算前拒绝。文件读取失败清除旧表示长度、范围、编码和验证器，返回 no-store 纯文本错误。
- Windows 新增 4 项测试，覆盖短输入/增长边界、小文件与流式路径替换、截短错误、范围句柄一致性及非法范围。完整 workspace/all-features/locked 回归通过（标准包 269 项、directives 19 项、file_conditions/file_variants 3 项），全 feature/all-targets 严格 Clippy 与 xtask 通过；默认忽略的测试未执行。
- 已打开句柄不是不可变快照；同 inode 原地覆盖、打开前的恶意目录/符号链接替换仍需后续平台原子路径约束。Windows 本轮已实际执行持有读句柄后重命名替换的测试；其他平台未执行。

第五十三批（2026-09-07，WEB-03 断点续传输入边界）：

- Range 与 If-Range 不再只取第一行：重复字段保守忽略范围、发送完整表示。If-Range 使用共享强标签比较，即使双方弱标签文本相同也不允许续传；日期 If-Range 尚未实现，仍返回完整文件。
- 范围位置严格限定十进制数字，不接受 Rust 数值解析原先允许的加号和内部空白；超大合法数字使用饱和计算，末端/后缀按文件长度截取，超大起点判不可满足。范围单位大小写不敏感，反向范围按非法输入回退完整响应。file-server 版本更新为 6。
- 新增 2 项边界单测，并将真实 HTTP 条件测试扩展到缓存关闭/开启各 24 个组合，覆盖重复字段、弱续传标签、加号、反向范围与大小写单位。Windows 标准包 271 项、directives 19 项及 file_conditions/file_variants 3 项通过；全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。本批未重跑完整 workspace 或跨平台验收。

第五十四批（2026-09-07，WEB-01 有界目录浏览）：

- 目录浏览移到独立 file_listing 模块；链接按 UTF-8 路径段进行百分号编码，显示文本单独做 HTML 转义，链接显式使用 ./ 前缀。修复空格、#、%、& 等字符生成不可用链接的问题，中文文件和目录可以通过生成链接实际访问。
- 列表条目解析真实路径并检查根目录包含关系，只列普通文件/目录，跳过隐藏名、控制字符名、不可 UTF-8 往返名称、根外/断开的符号链接。目录读取改用异步 API，不再在请求执行器里同步遍历整份目录。
- 每次最多扫描 10,000 个条目、生成 2 MiB HTML，隐藏项也计入扫描预算；超限返回 no-store 503，不返回静默截断的列表。file-server 版本更新为 7。可配置分页与原子路径隔离仍待后续实现。
- 新增链接编码/扫描与输出预算单测、Unix 符号链接过滤单测及真实 HTTP 链接往返测试。Windows 标准包 273 项、directives 19 项、file_conditions/file_variants/file_listing 4 项测试通过，全 feature/all-targets 严格 Clippy、xtask 和 whitespace 检查通过；Unix 符号链接测试及完整 workspace 本批未执行。

第五十五批（2026-09-07，WEB-01 静态服务方法与 Windows 名称策略）：

- file-server 明确只支持 GET/HEAD，其他到达 terminal 的方法在路径选择前返回 405、Allow: GET, HEAD 与 no-store；不再通过 POST/DELETE 等返回文件或目录列表。更早的 CORS guard 仍可处理预检。
- URL 解析与目录浏览共用 public_component 名称策略；所有平台拒绝隐藏/控制字符名称。Windows 额外拒绝冒号数据流、设备别名（包含扩展名和 COM/LPT 上标数字）、保留字符与结尾点/空格，依据 [Microsoft 文件命名说明](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file)。file-server 版本更新为 8。
- 新增 2 项名称策略单测、2 项真实 HTTP 测试：缓存下 6 种非读取方法 × 文件/目录/缺失路径均为 405，GET 缓存仍命中，HEAD 正常；Windows 实际创建并读回 NTFS 备用数据流，确认 HTTP 明文/编码路径、设备名和结尾别名均拒绝。
- Windows 标准包 275 项、directives/guards 28 项及 file_access/file_listing/file_conditions/file_variants 6 项通过，全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过；本批未重跑完整 workspace 或其他操作系统。原子路径约束、原地修改快照、Windows 短文件名别名等仍不属于已证明的安全边界。

第五十六批（2026-09-07，WEB-01 自动重定向同源边界）：

- 文件目录重定向与 trailing-slash 共用 local_redirect 构造器。请求路径的多重前导斜线收敛为单斜线，反斜线等路径特殊字符编码，避免生成 //host 形式的站外目标；空路径规范为 /。显式配置的 redirect 仍允许操作者指定站外地址。
- 文件目录重定向不再丢失查询参数，保留重复参数和已有百分号编码；trailing-slash add/remove 同样应用本地路径约束。file-server/trailing-slash 版本更新为 9/2。
- 新增 2 项构造器单测，使用 URL 解析验证攻击形态仍保持原 origin；新增真实 HTTP 测试覆盖目录/add/remove 三种模式共 9 个目标，断言状态、Location、查询参数与编码路径。Windows 标准包 277 项、directives/guards 28 项及 file_access/file_listing/local_redirects 4 项通过；全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。本批未重跑完整 workspace 或跨平台验收。

第五十七批（2026-09-07，CGI-03 / QA-02 SCGI HTTP/2 取消验收）：

- 补充独立 SCGI wire peer 的两项真实 H2 验收：未知长度多帧上传转准确长度和协议元变量、同连接额度拒绝、响应头前/后 RST_STREAM 归还 concurrency=1、同连接继续成功请求。测试区分上传半关闭和 socket 关闭，并探测取消后的后端输出方向。
- 后端首个响应不主动结束，测试 10 秒期限短于后端默认期限，避免正常完成或超时掩盖取消失效。共享 CGI 适配层无需实现变更即通过；未将此证据扩展为子进程 CGI 或真实 SCGI 应用互操作已完成。
- 修正 CGI 文档首页及局部段落的过期状态，补入 H2 证据。Windows gatel-cgi-http 单测/配置 13 项、gatel-scgi 交换 9 项、HTTP scgi/scgi_h2/fastcgi_h2 共 7 项通过；全 feature/all-targets 严格 Clippy、xtask、whitespace 检查通过。H2 热更新、Unix socket、真实应用服务器与子进程生命周期仍待推进。

第五十八批（2026-09-07，CGI-03 / RT-05 SCGI H2 热更新生命周期）：

- 新增真实 H2 + SCGI 热更新验收：未结束的 CGI 响应跨 prepare 失败和成功 reload，旧连接继续使用旧配置，新连接使用新配置；EOF 后正文完整，关闭旧连接释放旧 generation，新连接继续工作。
- 本次按代码和 ARCHITECTURE 中“连接固定 generation”契约验收，没有改成逐 stream 重选 generation；配置强制退役/长连接更新时限仍需后续独立方案，不能将该测试解释为旧连接即时应用新配置。
- Windows scgi_h2 现 3 项通过，scgi/reload/plugin_lifecycle 回归通过；全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。此批是现有共享 CGI/事务内核的验收补齐，不代表子进程 CGI、Unix 或真实应用服务器互操作完成。

第五十九批（2026-09-07，CGI-01 / CGI-03 请求环境一致性）：

- 修复特殊变量 CONTENT_TYPE 在 Connection 排除规则之前生成的问题，Connection 指定的 Content-Type 现在不进入环境；Connection 全部字段的 token 严格校验，拒绝非法文本。派生 CONTENT_LENGTH 仍来自已确认上传长度，不信任请求头原文。
- 拒绝重复 Content-Type、Host 和启用传递的 Authorization；未启用凭据传递时仍直接过滤认证头。重复 Cookie 与普通列表头分别使用原有的分号/逗号规则并新增顺序断言，错误字符串不携带凭据值。
- 共享 Config 指纹加入 gatel-cgi-http-config-v2 行为域，涵盖所有使用该 wrapper 的协议插件，不依赖逐个插件手工修改元数据版本。
- 新增 3 项映射单测并扩展独立 SCGI HTTP 后端测试，验证非法映射在连接前拒绝、特殊/普通排除字段、Cookie 和普通多值列表。Windows gatel-cgi-http 单测/配置 16 项、HTTP fastcgi/fastcgi_h2/scgi/scgi_h2 共 12 项通过（真实 PHP-FPM 项默认忽略）；全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。本批未重跑完整 workspace、真实应用或跨平台验收。

第六十批（2026-09-07，CGI-01 / RT-05 请求元数据提前准入）：

- 共享 CGI terminal 将环境校验移到并发 permit 和正文消费之前，避免非法路径/头的未知长度请求占用缓冲额度等待上传。完整缓冲后只更新派生 CONTENT_LENGTH，实际上传/连接期限与长度验证不变；共享 Config 行为域更新为 v3。
- Environment 新增显式 replace_existing，仅替换已有值，严格保持 NUL 与总预算检查；失败不改变环境和计数，insert 的重复名称拒绝保持不变。新增原子失败、增长/缩短与重复插入回归。
- 新增真实 HTTP/SCGI 准入测试：只发 chunked 请求头、不发任何正文，三类非法元数据均在 1 秒内返回 400，后端仅收到最后的有效 GET，唯一额度可继续使用。该期限显著短于配置的 30 秒总期限。
- Windows CGI 协议与共享适配器单测/集成共 32 项、HTTP cgi_admission/fastcgi/fastcgi_h2/scgi/scgi_h2 共 13 项通过（真实 PHP-FPM 项默认忽略）；全 feature/all-targets 严格 Clippy、xtask 与 whitespace 检查通过。本批未重跑完整 workspace 或跨平台验收。

第六十一批（2026-09-07，CGI-01 / RT-02 预缓冲空闲期限）：

- 未知长度 CGI 预缓冲每次正文读取应用 idle-timeout，空闲返回 408；外层总期限不变，持续小量进展不能无限延期。缓冲与后端交换现在共用配置的 idle 策略，但仍是读取/回调等待上限而非 packet 级活动计时；共享 Config 行为域更新为 v4。
- 新增虚拟时间测试覆盖完全停滞、持续进展重置 idle、总期限独立截止；新增真实 HTTP/SCGI 测试利用 100-continue 确认缓冲占用 concurrency=1，验证并发 503、空闲 408、后续额度复用和超时前不拨后端。
- Windows gatel-cgi-http 单测/配置 18 项、HTTP cgi_admission/fastcgi/fastcgi_h2/scgi/scgi_h2 共 14 项通过（真实 PHP-FPM 默认忽略）；全 feature/all-targets 严格 Clippy 与 xtask 通过。Windows 完整 workspace/all-features/locked 回归通过；默认忽略项、跨平台和真实应用互操作未执行。

第六十二批（2026-09-07，CGI-01 / QA-02 Unix 实际验收）：

- 新增 `cgi_unix.rs` 两项 Unix 专用 HTTP 集成测试，分别验证 FastCGI 记录完成和 SCGI netstring/上传半关闭；两者都验证缺失 socket 返回 502 后，同一代配置、唯一并发额度可以恢复处理 POST。
- Linux Docker（Rust 1.98.0）中单独执行两项新测试，均通过。标准功能包 279 项也通过，包含此前 Windows 跳过的辅助文件与目录列表符号链接隔离测试。已有 CI 的 Linux/macOS workspace 测试会自动纳入新文件。
- 完整 Linux `cargo test --workspace --all-features --locked --quiet` 已通过；下一轮收尾复核取得命令退出码 0 和容器 `exited 0`，解除上一轮退出状态待确认。默认忽略项未执行。Windows `xtask`、`git diff --check` 通过；Windows 对新文件运行 0 项，不能算 Unix 验收。
- macOS 实际执行、真实 SCGI 应用服务器、PHP-FPM 的 Unix socket 互操作，以及进程式 CGI 插件仍待完成。测试协议端不冒充生产服务器认证。

第六十三批（2026-09-07，CGI-01 / CGI-04 后端扩展边界）：

- 将网络拨号从共享 HTTP terminal 移入网络 Backend。新增 BackendFactory / BackendConfig / Backend / Invocation 和 `backend_directive`；非网络后端不再需要伪造 socket，输入准入、缓冲、容量、输出背压、绝对总期限和响应流取消仍由共享层管理。现有 Protocol 入口作为网络适配器保留。
- 配置验证不 prepare，prepare 不启动请求工作；每次启动与交换由同一个受保护 future 持有。网络拨号仍受 5 秒和总期限双重限制；配置指纹域升级 v5。进程树终止/回收仍需平台实现，未宣称进程式 CGI 已可用。
- 新增 4 项无网络后端契约测试：自定义目标语法与 decode 隔离、完成/失败/panic 释放、启动阶段超时/取消，以及响应流持有与释放资源。Windows 共享适配器 19+3 项、HTTP CGI 14 项通过（另 1 项外部 PHP-FPM 默认忽略）；全 workspace Clippy、xtask 和 diff 检查通过。
- 最终源码在 Linux Docker 中再次执行全 workspace / all-features / locked 测试，退出码 0；包括 19 项共享适配器单测、3 项配置测试与 Unix socket 集成。默认忽略的 soak、外部 PHP-FPM 和 doctest 未执行，不扩大为这些场景的验收证据。

第六十四批（2026-09-07，EXT-03 / CGI-04 协作关闭基础）：

- SDK 新增 Shutdown / ShutdownSignal 与 BackgroundTask::cooperative。显式 shutdown 统一通知所有任务，协作任务共享 5 秒清理预算，随后强制 abort 并 join；普通任务保持立即取消。成功清理与强制取消具有不同结果状态。
- 并发 shutdown 不再提前返回；关闭调用被取消时 abort 并保留剩余 join handle，后续调用继续 join，避免脱管。最终 generation Drop 仍同步 abort，不冒充异步进程回收；退役回收作用域和真实进程插件继续待办。
- 新增 6 项回归：通知不丢失、多任务同时清理、共享预算、并发关闭等待、关闭取消后重试，以及真实插件事务/LKG 后异步清理。Windows SDK 6 项、任务管理 9 项、真实插件生命周期 2 项通过。
- 已核查跨平台进程包装候选，将进程树/配额/退役/退出状态的剩余要求记录到 CGI 文档；没有加入未使用的进程依赖或宣称 CGI-04 完成。
- Windows 全 workspace / all-features / locked 测试通过；Linux 首轮在与 Windows 全量并行时发生 SCGI 热更新 8 秒测试超时（该套耗时 42 秒），未改 SCGI 代码或放宽期限，随后独立复跑全 workspace 退出码 0。资源竞争为可能因素，单次复跑不能证明其为唯一原因，保留为稳定性观察项。
- 全 workspace 严格 Clippy、xtask、diff 检查通过；独立 fuzz 锁文件补充 SDK 的 Tokio 依赖，locked/offline 元数据解析通过，未执行 fuzz。默认忽略的 soak、外部 PHP-FPM 和 doctest 本轮未运行。

第六十五批（2026-09-07，EXT-03 / CGI-04 有界退役作用域）：

- 新增 Server 所有的 Retirements。包含后台任务的配置在 prepare 预留容量，最多 64 个候选/活动/连接固定/退役中 generation；满额拒绝候选而不破坏 LKG，无后台任务不占名额。失败候选保持惰性并立即归还名额。
- PluginTasks 的 generation 所有权与 TaskScope 清理所有权分离；最后持有者释放后，宿主只保留任务作用域并运行协作 shutdown，不保留旧 Runtime/路由。清理完成才归还容量；Server shutdown 关闭注册，并发等待所有登记作用域与清理 jobs，即使仍有外部引用。
- 退役 job 的 join handle 跨关闭取消保留；独立作用域或宿主紧急 Drop 仍是 abort 兜底，不宣称 OS 进程已回收。进程式 CGI 的逐调用配额和平台 kill/reap 仍待实现。
- 新增 6 项回归：容量预留/候选惰性、清理占用容量、仍被引用的作用域并行退出、管理器关闭取消后重试、真实热更新释放旧 Runtime 后完成清理，以及 64 代容量耗尽保持 LKG/释放后重试成功。Windows 任务相关单测 13 项、真实插件生命周期 4 项通过；严格 Clippy 与 xtask 通过。
- 本轮 Windows 与 Linux Docker 分开执行全 workspace / all-features / locked 测试，均退出码 0；Linux 包含 65 项 HTTP 核心单测及 Unix socket 回归。默认忽略的 soak、外部 PHP-FPM 和 doctest 未执行。diff 检查通过；架构及插件文档已同步所有权、容量与紧急退出边界。

第六十六批（2026-09-07，EXT-03 / CGI-01 统一插件准备与 terminal 身份修复）：

- 新增 SDK PreparedDirective 和 DirectiveConfig::prepare，按 owner/候选 generation 一次返回处理器与惰性后台工厂；复用跨 listener/继承 route/fallback 的处理器。旧 build/background 通过默认 prepare 适配，失败候选统一释放资源。同一 owner 对应冲突配置时拒绝准备。
- 发现并修正 terminal 没有绑定 route owner 的缺陷，防止不同路由的同名 terminal 错误共用状态和任务去重身份。已有错误状态别名不兼容保留；站点共享配额应显式配置。
- CGI 接入统一 prepare，并增加 Backend::background 默认空钩子，让后续监督器与请求入口共享准备出的后端资源；哈希域升级 v6。仍未宣称进程式 CGI 已实现。
- 新增 3 项回归：terminal owner 路由隔离与稳定性、真实插件的跨 listener/route/fallback 资源复用及候选回滚，以及 `/a` 满额不阻塞 `/b` 的真实 CGI 配额隔离。相关测试、严格 Clippy 与 xtask 通过。
- Windows 与 Linux Docker 的全 workspace / all-features / locked 测试均退出码 0。Linux 本地 ACME 集成组耗时 83.43 秒且 Docker 查询曾短暂等待，最后 8 项均通过，慢运行原因未确定。默认忽略的 soak、外部 PHP-FPM 和 doctest 未执行；fuzz locked/offline 元数据解析、diff 检查通过，未运行 fuzz。

第六十七批（2026-09-07，RT-04 / EXT-03 宿主关闭取消安全）：

- 修复 Server shutdown 在 await 前取走全部监听器 join handle 的脱管窗口。等待期间 handle 保留在宿主容器内，完成后才移除；关闭 future 被取消后，重试仍等待原来的在途连接。
- shutdown 开始后 Server sync 不再激活监听器或插件任务。紧急 Server Drop abort 活动及退役 accept loops，并由其拥有的连接集合释放连接；不冒充异步插件清理。
- 新增 2 项真实 TCP 回归：取消两次 shutdown 后仍正确 drain 响应且无法 sync 重启，以及紧急 Drop 不遗留等待 30 秒 grace 的连接。关闭测试 4 项、插件生命周期 4 项、热更新 5 项通过；HTTP 全目标严格 Clippy 与 xtask 通过。
- 全 HTTP 首轮中两项关闭测试在初始配置提交前失败，单独复跑通过。补充初始 Outcome 断言以保留下一次失败原因；测试 harness 的临时目录名加入原子序号，消除并行同标签测试依赖系统时间唯一性的潜在竞争。尚不能确认该竞争就是首轮失败原因。
- 后续复跑发现 bind 单测连续申请临时地址时可拿到相同端口，使两个地址的前提失效。测试分配器记录已用地址，并暂时持有重复分配以迫使下一次选择其他端口；冲突夹具先占用端口再选择可用地址。4 项绑定测试通过，生产 bind 实现未改动。
- 最终 Windows `cargo test -p gatel-http --all-features --locked` 全套退出码 0；全 workspace/all-features/all-targets 严格 Clippy、xtask、diff 检查通过。本轮未重跑 Linux 或全 workspace 测试，不沿用上一轮结果冒充当前验收；默认忽略项未执行。

第六十八批（2026-09-07，CGI-04 进程 stdio 交换核心）：

- 新增独立 plugins/cgi 包，stdio 与退出等待由一次交换拥有，并行处理精确长度上传、CGI stdout、受限 stderr 及退出状态。stderr 只计数丢弃，默认 64 KiB；安静 stderr 不被误判为空闲失败。
- 输出最后一个字节等待 stdout EOF、stderr EOF 和成功退出后才提交。退出失败、长度错误、stderr 超限或超时都不 finish；取消关闭管道并丢弃输入/退出等待，但仍必须由监督器 kill/reap 进程树、持续持有进程配额。
- Windows 新包 8 项回归通过，包括输出四阶段故障/背压/总期限矩阵，以及成功退出后仍等待 stderr EOF。全 workspace/all-features/all-targets 严格 Clippy、xtask、diff 检查通过，补测后新包 all-targets 严格 Clippy 再次通过。
- Linux Docker（rust:1-slim）完整 `cargo test --workspace --all-features --locked --quiet` 通过，包含上述 8 项；默认忽略的外部互操作、长稳与示例测试未执行，macOS 未验收。默认宿主生产依赖树不包含 gatel-cgi；尚无 bundle、宿主 cgi feature、程序启动及进程树回收，未把协议夹具标为真实进程验收。

第六十九批（2026-09-07，CGI-04 代际有界监管与取消后清理）：

- 新增 `gatel-cgi::supervisor`，提供惰性 Client/Job/Supervisor 与 SDK 协作后台任务接入；容量同时覆盖排队、执行和清理，取消请求不提前释放额度，不自行 spawn/detach。
- 所有执行退出路径进入 cleanup；清理错误/panic 关闭该代接收入口，再取消其余工作。正常退出并行清理所有任务，完成结果仅在清理结束后交付；未激活候选销毁关闭入口。
- Windows 8 项交换测试和新增 6 项监管测试通过；全 workspace/all-features/all-targets 严格 Clippy 通过。可控 Job 仅证明调度与所有权契约，真实 Unix/Windows 进程启动、kill/reap、Drop 紧急终止及宿主可选插件接入仍待实现。
- Linux 首轮全量在缓存集成测试出现一次未提交初始配置的失败，原测试未断言 apply 结果，根因未证实；补充原始 Outcome 断言后，Windows 对应测试与 Linux 全 workspace/all-features/locked 复验通过。复验通过不代表该间歇问题已根治，保留诊断断言继续追踪；xtask、diff 检查通过，默认忽略项和 macOS 未验收。

第七十批（2026-09-07，CGI-04 Linux 真实进程组原语）：

- 新增 Linux `Process`：绝对程序/cwd、显式参数、清空继承环境、三管道与独立进程组；`exited()` 通过 waitid/WNOWAIT 保留组长的 waitable 状态，组信号完成后才回收直接子进程，避免回收后继续使用旧数字 PGID。
- 显式 terminate/reap 与 Drop 紧急组终止均已实现；不使用 detached blocking waiter。仅新增 Linux 目标的 nix/process/signal 与 Tokio process 依赖，默认宿主没有接入此包。
- Linux 5 项真实进程测试通过：独立环境和管道、失败退出码及非消耗式观察、取消后组终止/直接子进程回收、Drop 关闭后代继承的管道，以及启动前拒绝相对程序/cwd。管道 EOF 不替代所有后代退出/回收的证明；该原语尚未组装为平台 Job，Windows/macOS、严格后代 containment 和宿主 feature 仍待实施。
- Linux Docker 完整 `cargo test --workspace --all-features --locked --quiet` 退出码 0；Windows 全 workspace/all-features/all-targets 严格 Clippy、xtask 与 diff 检查通过。Windows Clippy 不覆盖 cfg(linux) 源码；Linux Clippy 首次缺组件，已安装但本批未运行完成检查。默认忽略项未执行。

第七十一批（2026-09-07，CGI-04 Windows Job Object 真实进程边界）：

- 新增 Windows Process 实现：挂起创建、执行前纳入查询 Job 和终止 Job、禁止 breakaway、kill-on-close、显式终止后验证整个 Job 活动进程归零。使用安全依赖接口，无本包 unsafe FFI，无 detached 阻塞 waiter。
- 仅接受绝对 exe/cwd、显式参数和 UTF-8 环境，避免继承宿主环境或隐式 cmd/bat。Windows 5 项真实场景加 1 项夹具入口通过，已有 8 项交换/6 项监管测试通过；全 workspace/all-features/all-targets 严格 Clippy、xtask、diff 检查通过。
- 尚未将平台 Process 与交换/监管组合为实际 CGI Job，后代数量资源限制和宿主可选 feature 仍待实施。Linux 进程组与 Windows Job 的完成保证不同，不以 Windows 验收替代 Linux 的全部后代回收证明。
- 最终 Windows `cargo test --workspace --all-features --locked --quiet` 退出码 0。Linux CGI 19 项测试及该包 all-targets 严格 Clippy 通过（补齐上一批 Linux 静态检查缺口）；本批未重跑 Linux 全 workspace，macOS 和默认忽略项未验收。默认宿主生产依赖树不包含 gatel-cgi、process-wrap 或 win32job。

第七十二批（2026-09-07，CGI-04 Windows 真实调用链）：

- 新增 Windows `job::Invocation`，惰性 Program/Request 经 supervisor 启动真实进程，连接 stdio 交换器；绝对 deadline 覆盖排队/启动/传输，超预算与已过期请求在启动前拒绝。
- 正常退出也先清理整个 Windows Job，再放行响应尾字节和 finish；失败/取消继续由监管者持有进程，cleanup 受 2 秒上限约束，不能证明完成则封闭该代而不复用配额。
- 新增无 harness 污染的真实 CGI 集成可执行夹具，Windows 回显 100 KB、非零退出、stderr 超限、超时、过期、取消和清理后再次成功共 7 个顺序场景通过。新包其他测试、全 workspace/all-features/all-targets 严格 Clippy、xtask 与 diff 检查通过。
- 本批仍未接入 HTTP BackendFactory/bundle、脚本配置映射与后代数量限制；Linux 全作用域完成保证不足，未直接启用 Windows 适配，macOS 未实现。本批未运行全 workspace 测试或跨平台调用验收。

第七十三批（2026-09-07，CGI-04 Windows 可选 HTTP 插件接入）：

- `gatel-cgi::bundle()` 和宿主 `cgi` 可选 feature 接通 BackendFactory、固定脚本映射、prepare/background/代际退役及真实子进程；程序路径 canonicalize，无请求参数拼接、无附加命令行参数，不支持 remote-root。非 Windows 配置明确拒绝。
- BackendConfig 传递 concurrency，每代监管容量覆盖清理；新增 Unavailable 映射为 503，并修复响应头前输出通道关闭抢先吞掉具体后端错误的问题。共享配置哈希升级为 v7；增加 503/504/502 映射回归。
- 真实 Windows HTTP 测试通过 GET/POST/HEAD、失败/提前退出、超时后清理恢复、无效程序保留 LKG、替换 CGI 路由和 shutdown。新增能力/平台验证及示例。跨代进程额度共享、后代数量上限、Linux/macOS 子进程调用仍待完成，不宣称 CGI 全平台完整化。
- Windows 与 Linux `cargo test --workspace --all-features --locked --quiet` 全部退出码 0；Windows 全工作区严格 Clippy、默认关闭 feature 的宿主验证、xtask、diff 和 fuzz locked/offline metadata 检查通过。默认生产依赖不包含 gatel-cgi/process-wrap/win32job。Linux 的空平台测试入口不算 Windows 调用链验收，默认忽略项/macOS 未执行。

第七十四批（2026-09-07，CGI-04 兼容热更新跨代进程配额）：

- 共享 HTTP 适配层状态拆分为 HTTP/进程两套容量，通过 BackendFactory::prepare_with_capacity 交给进程后端。同一所有者及 concurrency 的兼容配置复用进程池；目标程序变化、失败候选均不绕过旧调用清理占额。配置哈希 v8，容量状态域 v2。
- supervisor 新增共享容量构造，队列/退役标记独立。普通退役和惰性候选销毁不关闭其他代；cleanup 失败/panic 或已开始工作被强制 abort 时关闭共享池，阻止未经确认回收的额度跨代复用。
- 新增 3 项监管回归及 1 项真实准备事务测试，Windows 9 项监管测试、跨代池身份/失败候选测试和真实 CGI HTTP 链路通过；严格全工作区 Clippy、xtask、diff 与 fuzz locked/offline metadata 通过。
- concurrency 改变或指令所有者变化仍建立新池，动态跨代预算调整未完成；后代数量限制和 Linux/macOS 完整子进程调用继续保留任务，不用兼容重载共享替代这些要求。
- Windows 与 Linux 的完整 workspace/all-features/locked 测试退出码均为 0；最后补充直接 prepare 入口先校验容量再构造 semaphore，并通过该包严格 Clippy。默认忽略项和 macOS 未执行，Linux 的空 Windows 平台测试入口不作为调用验收。

第七十五批（2026-09-07，CGI-04 Windows 进程数量上限与平台 FFI 边界）：

- 新增独立 gatel-process-windows 平台包，仅依赖 windows API，无 HTTP/配置/异步运行时依赖。私有 OwnedHandle、kill-on-close、不允许 breakaway、1～1024 活动进程限制以及固定大小活动计数查询；局部 unsafe FFI 有逐项安全说明，CGI 包仍 forbid unsafe。
- Windows CGI 默认每次 Job 上限 64，包含根进程和所有受控后代；底层允许显式设置，暂未开放 KDL 配置。移除 win32job 及其额外 windows 0.61 依赖，替代有固定 PID 列表容量的查询。
- 新增平台边界校验和真实两进程上限测试：第三个进程创建失败，清理后计数归零。初始测试误要求根进程退出后计数立即等于 1，实测不满足；调整为验证真正的完成条件——终止后计数归零，不以瞬时计数代替进程回收证明。
- 严格全工作区 Clippy、xtask、diff、fuzz locked/offline metadata 和默认宿主依赖裁剪检查通过；不宣称 Linux/macOS 进程上限或完整调用已完成。
- Windows 与 Linux 完整 workspace/all-features/locked 测试最终退出码均为 0。Windows ACME 单测阶段耗时约 67 秒，确认原进程完成后记录通过，未因等待而重启测试；默认忽略项和 macOS 未执行。

第七十六批（2026-09-07，CGI-04 可配置进程树上限与重载验收）：

- KDL 新增可选 process-limit，Windows CGI 默认 64、范围 1～1024，包含根进程和后代；经共享 BackendConfig、不可变 Program 到 OS Job，恢复程序执行前施加限制。FastCGI/SCGI 在 decode 和直接 prepare 都拒绝该字段，不静默忽略。
- 配置指纹升级 v9，修改上限会重新准备；容量兼容域仍为 v2，同 owner/concurrency 保留旧调用清理占额。旧调用继续使用旧上限，不做追溯调整。
- 新增指纹/容量兼容性与网络入口拒绝测试；真实 Windows HTTP 验证两进程限制、非法类型和越界保留 LKG、1/1024 边界、仅更改上限触发 reload 及再次执行限制。
- Windows 专项测试、全工作区严格 Clippy、xtask、diff 与 fuzz locked/offline metadata 通过；Windows 和 Linux 完整 workspace/all-features/locked 测试退出码均为 0。未执行 macOS 或默认忽略项；Linux 空 Windows 测试入口不算跨平台进程调用验收。

第七十七批（2026-09-07，OPS-01 CLI 补全/手册恢复与 BASE-01 命令审计）：

- 对照本地 main eaf9bde 的实际 cli.rs 恢复 completions 和 man-page；直接使用当前 Clap 命令树，补全支持 Bash/Elvish/Fish/PowerShell/Zsh，手册增加子命令路径。不加载配置、不启动异步宿主或安装系统文件。
- 输出先生成到内存，再统一 write/flush；BrokenPipe 正常结束，其他写入错误返回 3，未知 Shell/手册路径返回 2，错误不混入 stdout。
- 新增真实二进制测试，覆盖五种 Shell、全部当前业务子命令、选项与默认值、确定性、非法路径和输出纯净；单测验证 write/flush 失败。初次断言误忽略 roff 连字符转义，按实际格式修正后通过，未改变生成内容掩盖问题。
- 新增 docs/CLI.md 和增量 docs/compat-v1.md：逐项列出旧 CLI 参数/默认值、Windows service 和 passwd/precompress 工具，以及当前缺失与 breaking 差异。BASE-01 未完成，仍需配置解析器、全部示例和差分运行审计。
- Windows 默认及全 feature 宿主测试、全工作区严格 Clippy、xtask、diff、fuzz locked/offline metadata 通过；Linux 完整 workspace/all-features/locked 回归退出码 0。额外通过 PowerShell AST parser 与 Linux Bash -n 补全语法检查；本批未重跑 Windows 全工作区测试，不把生成/语法检查等同于所有 Shell 的交互安装或 man 渲染验收。

第七十八批（2026-09-07，OPS-01 serve 命令与不可变配置前端）：

- 恢复 serve ROOT、-p/--port、-l/--listen、-b/--browse，并新增 --dry-run。默认根为当前目录、端口 8080、浏览关闭；有意将默认监听改为 127.0.0.1，外部访问需明确 opt-in，监听仅接受 IP，端口 1～65535。
- App 增加不可变 Snapshot 输入，复用正常 normalize/compile/prepare/commit/activate/shutdown；serve 使用类型化 Node 和独立文件根，不拼 KDL、不生成临时文件、不读取/监控本地配置。静态文件仍走标准 handler；固定选项不热更新，内容变化直接读取。
- 新增真实 CLI HTTP 回归：GET/HEAD/Range、浏览开关、隐藏文件保护、内容更新、错误 gatel.kdl 不影响启动且不被改写、dry-run、端口占用、非法参数/根目录。额外验证不可变输入正常编译、事务 NoOp 和关闭；补全/手册自动包含新命令和默认值。
- 初始夹具使用端口 0，被正常配置校验拒绝；保持内核不变量，CLI 同样拒绝 0。Linux 首轮 SIGTERM 夹具依赖容器缺失的 kill 命令，改用仅 Unix dev-dependency 的 nix 信号接口；随后三项 Linux serve 场景通过，包括 SIGTERM 成功退出和特殊/非 UTF-8 目录路径。
- Windows 与 Linux 完整 workspace/all-features/locked 回归退出码均为 0，Windows 严格全工作区 Clippy、Linux 宿主 all-feature/all-targets 严格 Clippy、xtask、diff、fuzz locked/offline metadata 通过。Linux ACME 阶段耗时约 73 秒，等待原测试完成，未重启或放宽断言。macOS 与真实 Windows 控制台 Ctrl+C 未执行，未将 Windows 强制结束测试进程当作优雅关闭证明。

第七十九批（2026-09-07，OPS-01 passwd 与真实认证衔接）：

- 恢复为 gatel passwd --stdin，使用现有 bcrypt，不接收密码参数或回显式终端输入；cost 默认 12、范围 4～16。只移除末尾一次 LF/CRLF，保留空格；拒绝空值、多行、NUL、非 UTF-8。
- 使用当前 bcrypt non_truncating_hash，按其终止字节规则接受 1～71 字节；最多读取 74 字节，Zeroizing 保护命令输入缓冲，启用 bcrypt 内部 zeroize。错误不携带输入材料，输入/哈希/输出失败返回 3，参数错误返回 2。
- 新增输入边界、读取失败、无限输入、随机盐及 CLI 错误码测试；真实 CLI 生成哈希写入 Secret 文件后，运行网关验证带空格的正确密码返回 200、去掉空格和空授权返回 401。CLI 帮助、补全和手册沿用同一命令树。
- Linux 首轮真实 HTTP 夹具因 Secret 文件默认权限不符合现有策略而启动失败；改为创建时设置 0600，并在 CLI 文档说明 umask 与现有文件权限，不放宽 Secret 加载器。随后 Linux 完整 workspace/all-features/locked 回归退出码 0。
- Windows 默认/全 feature 宿主测试、标准包 277 项、全工作区严格 Clippy、xtask、diff、fuzz locked/offline metadata 通过；本批未重跑 Windows 全工作区或 macOS 测试。交互式隐藏输入、旧独立二进制、多算法和认证请求侧成本/并发预算仍未完成，不将本轮标为 AUTH-01 整体完成。

第八十批（2026-09-07，AUTH-01 bcrypt 工作预算与严格输入）：

- bcrypt 从异步请求线程移至 spawn_blocking，进程级共享 16 项 Semaphore 覆盖所有路由/App/新旧 generation。先 try_acquire 再提交，无无限认证排队；容量或执行故障返回 503。额度由实际计算闭包持有，请求取消不提前归还，panic 释放；正常运行时销毁仍可能等待已启动的阻塞计算。
- prepare 不做哈希计算，先校验规范 bcrypt 格式、未用编码位和 cost 4～16；最多 1024 用户，用户名最多 256 字节。新配置成本过高/损坏会拒绝且保留 LKG。配置行为域更新为 gatel-basic-auth-v2。
- 编码凭据最多 512 字节、密码最多 72 字节，拒绝重复 Authorization、错误 padding/非规范 Base64、NUL/CR/LF/非 UTF-8。避免长密码通过 bcrypt 截断匹配已有前缀；Basic scheme 大小写不敏感。未知用户同样做一次验证，但不宣称不同成本用户间恒定耗时。
- 新增单线程异步执行器/阻塞夹具的取消保额、超额拒绝、完成恢复、panic 回收测试，以及 503、长度/重复头/编码测试；真实 HTTP 验证昂贵或损坏哈希 prepare 失败且旧认证仍返回 200。新增 docs/AUTH.md 明确不可中断计算、固定预算、旧连接凭据与后续撤销边界。
- Windows 完整 workspace/all-features/locked 回归退出码 0；严格 Clippy、xtask、diff、fuzz metadata 通过。Linux 首轮在无关缓存夹具取地址前未成功提交而失败：补充提交结果断言，并先绑定后端再选网关端口，消除同夹具内可见的端口复用风险；原错误没有事务原因，不能断言这就是唯一根因。随后 Linux 完整 workspace/all-features/locked 回归退出码 0，Windows 缓存 23 项也重新通过，不降低缓存断言。macOS、默认忽略项与长稳负载未执行。

第八十一批（2026-09-07，AUTH-01 共享密码策略包与可选 Argon2id）：

- 新增 gatel-password，不依赖 HTTP、配置或 Tokio；从标准指令迁移 bcrypt 策略，提供脱敏、不可变 Credential 与共用生成/验证接口。主机和标准包不再直接实现密码验证策略，保留统一阻塞工作额度和 SecretRef 边界。
- 默认关闭 argon2 feature，同时控制算法依赖、CLI 候选和能力声明；启用后仅支持 Argon2id v19、明确 m/t/p、16 字节盐/32 字节摘要。m=8192～32768 KiB、t=1～4、p=1～4，先验证预算再计算，不接受附加 keyid/data。生成固定 m=19456/t=2/p=1，拒绝套用 bcrypt --cost。
- 两种算法共享 16 项额度；Argon2id 矩阵参数合计上限 512 MiB，不宣称进程 RSS 上限或墙钟期限。HTTP 密码总体上限扩为 256 字节/编码 1024 字节，bcrypt 自身仍拒绝超过 72 字节；CLI 生成分别限制 71/256 字节。
- 配置行为域升级 v3，并纳入实际算法列表；直接内联 PHC 哈希拒绝并脱敏，不误作文件路径。新增生成/验证/参数拒绝/默认关闭/Debug 脱敏测试，真实 CLI → Secret → HTTP 验证 bcrypt 与 Argon2id，以及能力与选项一致性。
- Windows 默认及全 feature 宿主/密码包测试、Windows/Linux 完整 workspace/all-features/locked 回归、严格全工作区 Clippy、xtask、diff 通过；fuzz lock 同步且 locked/offline metadata 通过。默认宿主正常依赖树不含 argon2/password-hash/phc。末尾 PHC 脱敏变更在 Windows/Linux 各补测 17 项认证单测通过。macOS、默认忽略项、独立实现互操作和长稳负载本批未执行。
- scrypt/PBKDF2、隐藏交互、可配置总内存/CPU预算、公平调度及紧急凭据撤销仍待实施，未将 AUTH-01 或整体目标标为完成。

第八十二批（2026-09-07，AUTH-01 / QA-02 独立密码互操作与解析健壮性）：

- 新增 PHP 8.4.25 独立生成的 Argon2id 固定向量；开启/关闭 feature 均测试明确的接受/拒绝行为。另增显式忽略的 Docker 双向互操作测试，使用固定镜像摘要、禁网和资源限制，覆盖 PHP 验证 Gatel 输出、Gatel 验证 PHP 输出以及密码空格语义；本地已显式运行通过 bcrypt 与 Argon2id 两项流程。
- 新增纯解析参数矩阵、溢出/缺失/重复参数、盐/摘要/版本边界测试；共享 fuzz harness 覆盖规范 bcrypt 与 Argon2id 种子、逐字节变异、截断、长输入、Debug/错误脱敏，绝不执行攻击者指定的密码计算。
- 新增 `password_policy` fuzz target 并加入 CI 矩阵；独立 CI 门禁检查默认宿主不链接可选密码算法，并显式运行固定 PHP 镜像互操作。CI 配置已落地，不代表远端工作流已执行。
- Windows 默认及 all-features 密码测试、全 workspace/all-features/all-targets/locked 回归、严格 workspace Clippy、rustfmt、xtask、diff 和锁文件一致性检查通过。CGI 模块声明仅按 rustfmt 调整顺序。
- Linux 隔离容器使用 nightly 1.100.0（2026-09-06）与 cargo-fuzz 0.13.2，默认 sanitizer，运行 `password_policy -- -max_total_time=30 -max_len=4096`：31 秒完成 1,075,280 次输入执行，退出码 0，无崩溃或断言失败。空 corpus 起步，结束时 159 项 corpus；这是短时冒烟而非长期 fuzz，亦非认证计算的内存/并发压力测试。多租户公平预算、紧急凭据撤销和长期压力测试仍待完成。

第八十三批（2026-09-07，EXT-03 / AUTH-01 / RT-05 宿主阻塞工作服务）：

- 将标准认证内部 Semaphore/工作模块替换为公共 `gatel-plugin-runtime::blocking`：宿主持有 Budget 控制器，插件仅接收 Executor；Server → Resources → BuildContext 注入同一执行句柄。默认各 Server/App/代共享 16 项任务、256 MiB 声明工作内存；嵌入式宿主可通过独立控制器与 `Server::with_blocking_executor` 配置预算。
- 任务数和声明内存原子准入，失败不扣任一额度，不创建准入等待队列；Tokio 阻塞队列及运行中任务都持有预约，取消等待者不能提前释放，完成/panic 后回收。调低上限保留真实占用；无效调限不改状态；关闭不可借调限重开。公开一致的 snapshot，但未加入观测导出。
- Credential 从已验证参数给出声明内存（Argon2id 的 m × 1024，bcrypt 8 KiB）；认证选定用户或替代凭据后申请对应额度，拒绝/关闭/执行失败返回 503、不发认证 challenge。认证行为域升级 v4。声明工作内存不是 RSS、分配器或线程硬限制，源码链接插件仍受信任。
- 六项 SDK 测试覆盖缩容/关闭、非法参数/溢出、运行中取消、排队时取消、panic 和多线程竞争。新增真实 HTTP 宿主注入测试：任务槽空闲但内存不足、成功重载/失败 prepare 保留在途使用量、调限及恢复、关闭后拒绝。旧私有工作模块测试已迁入 SDK，未保留两套执行边界。
- Windows 全工作区 all-features/all-targets/locked 与 Linux 全工作区 all-features/locked 回归通过；严格全工作区 Clippy、rustfmt、xtask 通过。本批未执行 macOS、长期负载或新一轮 sanitizer fuzz。
- 尚未将该 API 接到 KDL/CLI/Admin 配置与原子配置提交，不允许在候选 prepare 时修改共享预算；多租户公平性、CPU 时间计量、紧急凭据撤销与不可中断任务的硬关闭期限仍未完成。EXT-03、AUTH-01、RT-05 保持部分完成。

第八十四批（2026-09-07，RT-01 / CTL-02 配置事务版本与失败发布边界）：

- 在推进预算 KDL/提交联动前发现基础事务缺陷：旧快照使用合并后的新 head 作为自身版本，可能重新 prepare/发布旧内容；旧 NoOp 会清除新拒绝；过时 prepare 错误可能覆盖新失败或污染新成功的事务状态。先加入回归测试，原实现六项中五项失败，然后修复，未将预算配置接入标为完成。
- Snapshot 保持自己的完整 revision vector，合并观察头后立即比较，旧版本或缺失已知来源返回 Stale，不能借新头进入编译/prepare。明确来源贡献删除使用更高版本的空 SourceSnapshot，省略来源不是删除。
- 成功、NoOp、normalize/prepare 失败统一在锁内检查来源头与 base generation；过时失败不改 last_failure，不产生当前配置失败结果。版本计数耗尽也经过同一失败发布检查。两个同版本候选只允许一个发布。
- 文件宿主将读取/解析、版本分配与上一快照缓存更新放在同一观察锁中，防止并发读取复用版本或倒序覆盖缓存；不持有服务或 prepare 锁。读取失败保留已有游标。
- 新增八项事务回归（含受控跨线程 normalize 失败、迟到 prepare 失败、同版本并发和显式来源删除）、一项观察缓存锁/失败保留测试、一项真实 HTTP 迟到快照不回滚服务测试。
- Windows 全工作区 all-features/all-targets/locked、Linux 全工作区 all-features/locked 回归通过；严格 Clippy、rustfmt、xtask、diff 与 fuzz 锁文件检查通过。末尾来源删除测试增强及计数耗尽失败检查在 Windows/Linux 各补测全部 plan 测试（84 单元 + 6 扩展 + 62 规范化）通过。本批未执行 macOS、长稳负载或 sanitizer fuzz。
- 本批保证 GenerationStore 的事务状态，不宣称已解决外部异步 telemetry 消费顺序、所有前端错误的版本化、Admin CAS/持久化或共享预算的 commit-time 调限协议；这些仍是后续工作。

第八十五批（2026-09-07，RT-01 / RT-05 / AUTH-01 预算 KDL 与提交期调限）：

- 新增 `global { blocking jobs=8 memory="128MiB" }`，jobs 范围 1～1024、memory 非零，默认 16 项/256 MiB；重复节点、未知属性和非法值拒绝，数值参与指纹。删除 blocking 在下一次真实提交恢复默认，不清零占用。新增 examples/blocking.kdl，实际 CLI validate 通过。
- 新增宿主 PublishRuntime/PublishGuard 契约：资源准备仅生成已校验的 PreparedChange；候选通过 stale 检查后按 generation → budget 顺序加锁，发布新代后释放预算锁。未发布 guard 丢弃恢复限额；失败/stale/NoOp 不调限，预算关闭后不会因新配置重新开放。此协议不调用插件回调、不允许 I/O/await/重入；不是任意插件副作用的事务回滚引擎。
- CLI App 持有自己的配置预算并跨路由/代复用；低层 Server::new 仍保留固定进程级默认执行器。嵌入式宿主须明确提供匹配的 Resources.blocking_budget，否则显式 KDL 预算在 prepare 拒绝。多个独立 App/控制器不再宣称共用一个 16 项/256 MiB 总额；跨宿主共享需显式统一控制器及配置写入权。公开自定义运行时类型需实现 PublishRuntime。
- 新增五项测试：SDK 准备惰性/所有者检查/锁及回滚，事务只发布获胜候选，配置参数与指纹，两项 HTTP 测试覆盖旧 keep-alive 连接、在途缩容、失败候选、NoOp、删除恢复、关闭不复活与缺失控制器拒绝。配置测试拆入独立模块以保持文件规模门禁。
- Windows 全工作区 all-features/all-targets/locked 与 Linux 全工作区 all-features/locked 回归通过，严格 Clippy、rustfmt、xtask、diff 与 fuzz metadata 锁文件检查通过；末尾测试模块拆分和默认值去重在 Windows 完成相关测试、Linux 完成全 plan/runtime 补测。没有执行 macOS、长稳负载、新一轮 sanitizer fuzz 或远端 CI。
- Admin API 配置入口、公平调度、指标导出、紧急凭据撤销、CPU 时间计量和不可中断工作的硬期限仍待完成。声明内存仍不是 RSS 硬限制，本批不将整体目标或 AUTH-01 标记完成。

第八十六批（2026-09-07，OBS-02 / RT-05 / AUTH-01 阻塞预算运行观测）：

- 标准 metrics 终端追加同一宿主 Executor 的 13 条固定预算序列，覆盖排队/运行占用、声明内存、限额、关闭、准入、拒绝与工作结束。仅使用固定 reason/outcome 标签，不引入用户名、哈希、路径或动态插件名称。渲染层与运行时采样分离，嵌入式宿主可复用同一输出契约，详见 [METRICS.md](METRICS.md)。
- 准入拒绝按 closed → jobs → memory 互斥计数；计数饱和不回绕。任务守卫跟踪排队、运行和正常结束，HTTP 等待者取消不提前释放额度或漏记实际完成。错误密码的正常验证属于 completed，不误记为执行失败；panic 展开与未开始预约丢弃单独计数。
- 一次预算快照在同一锁下复制，调限、关闭及重载保留生命周期计数；不是全服务全局原子采样，也不是 RSS/CPU 时间计量。不自动暴露监控路由或绕过认证；受同一预算认证保护的监控路由仍可能返回 503。
- 新增指标渲染、计数分类/饱和和真实 HTTP 指标测试，并增强排队/运行等待者取消测试。HTTP 覆盖内存拒绝、缩容后任务拒绝、重载计数保留、关闭后独立监控可抓取和凭据不泄漏。标准 metrics 指令行为版本更新为 2。
- Windows 全工作区 all-features/all-targets/locked 重跑通过，Linux 全工作区 all-features/locked 通过，严格 Clippy、xtask、rustfmt 与 diff 检查通过。Windows 首轮 SCGI generation 测试失败，随后该套件单独重跑及全工作区重跑均通过；未放宽超时或断言，首次失败根因仍未确认。末尾新增排队计数断言通过 Windows runtime 15 项测试。
- 管理 listener、Admin API、公平调度、OTLP、紧急凭据撤销及 CPU 硬期限仍待实施；macOS、远端 CI、长稳负载和新一轮 sanitizer fuzz 本批未执行，不将 OBS-02 或整体目标标记完成。

第八十七批（2026-09-07，CTL-01 / CTL-02 原子条件写入基础）：

- 新增 Status::version 与 GenerationStore::attempt_if，版本包含 BootId、generation 和完整 observed head。前置条件与候选观察在同一写锁内完成，避免调用方先读状态再无条件写入的竞态；原有文件观察入口仍可无条件提交。
- 冲突返回独立 Conflict/current Version，不修改 head、失败状态或运行资源，不进入 prepare，也不作为配置失败计数。拒绝/NoOp 的新来源版本会使旧令牌失效；同一 head 的成功重试通过 generation 失效；跨 Store 生命周期通过 BootId 失效。准入后仍执行已有 stale/prepare/commit 契约，并非保证最终提交。
- 新增六项测试：冲突不污染状态、NoOp 意图更新、跨生命周期、同一 head 重试、真实双线程争写及准备期间被新观察抢先。Windows 全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy 与 xtask 通过；末尾错误类型及第六项测试另行通过 plan 的 91 项单测、69 项集成测试和严格 Clippy，rustfmt/diff 检查通过。
- 此为进程内控制面基础，不宣称 HTTP Admin API、ETag 编码、资源所有权、revision 分配或持久化已完成。下一步需把认证、监听边界、多来源变更与恢复协议接入同一事务链，禁止另起旁路状态写入。Linux/macOS、远端 CI 和长稳本批未执行。

第八十八批（2026-09-07，CTL-01 / RT-01 来源版本身份约束）：

- GenerationStore 为每个已知来源保留最新观察版本对应的 content 指纹，包括 prepare/normalize 失败和 NoOp。相同版本但不同指纹在编译前返回 Stale，不运行资源准备，不覆盖 LKG 或已有失败，不推进候选内其他来源；旧版本也不能污染最新指纹。仅存每来源最新指纹，不保留内容历史。
- 原内容重试仍合法；修改内容必须推进来源 revision。检查的是受信任前端提供的指纹身份，并非对伪造指纹/节点的沙箱；来源解析器仍须确保完整输入、指纹与不可变节点一致。错误不输出配置内容或指纹。完整契约已写入 ARCHITECTURE。
- 新增五项事务测试，覆盖已提交、失败、NoOp、混合来源部分推进、重试及准备期间同版本替换；增强真实 HTTP reload 测试，将旧内容重新标记为当前版本后仍被拒绝，新路由持续服务。
- Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 检查通过；末尾增强 HTTP reload 六项另行通过。Linux plan 96 项单测、69 项集成测试、runtime 15 项测试及 HTTP reload 六项通过，也补验第八十七批条件写入。本批未运行 Linux 全工作区、macOS、远端 CI 或长稳。
- Admin HTTP 层、来源所有权/容量、版本分配与持久化恢复仍未完成；不以版本检查替代授权或持久化承诺。

第八十九批（2026-09-07，CTL-01 / RT-01 来源分配与容量边界）：

- SourceRev::next 改为 checked increment/Option，修复 u64 上限的 debug panic/release 回绕。新增纯模型 SourceSnapshot::observe：校验 previous 属于同一来源，内容未变保留 revision，内容改变且版本耗尽则返回脱敏 ObservationError。KDL 已实际迁入该接口并转换为诊断；不是只提供未使用的工具。宿主仍负责锁住整个读取、分配和观察缓存更新过程。
- GenerationStore 默认允许生命周期内最多 256 个不同来源；嵌入式宿主可通过 with_source_limit 配置非零上限。超容量在观察前返回 Stale，不部分推进来源、不准备资源；已有来源仍可更新。失败来源和空 tombstone 仍计入容量，避免遗忘版本后旧内容复活。容量错误不能原样无限重试；这不是每来源字节/节点上限、所有权或租户配额。
- 新增四项测试：模型 revision/身份边界、真实 KDL 文件观察、容量原子性与失败/空来源身份保留。Windows/Linux 的 model 41 项、KDL 24 项、plan 98 项单测及 69 项集成测试通过；Windows 全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy、xtask、rustfmt/diff 与 fuzz metadata 锁检查通过。
- 公开 SourceRev::next 的返回类型是有意破坏性变更，所有当前调用点已经迁移。Admin API、来源注册/授权、持久化版本恢复与管理监听边界仍未完成；本批未运行 Linux 全工作区、macOS、远端 CI、新 fuzz 或长稳。

第九十批（2026-09-07，CTL-01 / CTL-02 宿主来源注册与全局配置归属）：

- 新增 GenerationStore::with_allowed_sources，固定宿主允许的精确 SourceId 集合；重复注册去重，空集合拒绝全部非空来源观察，容量等于注册身份数。未知来源在任何 head/指纹推进之前返回 Stale，不准备资源。原有低层动态来源模式仍保留数量上限。
- CLI 已实际接入：文件模式仅注册 file:main；不可变 Snapshot/serve 模式由宿主注册传入来源，不误拒绝 file:serve。注册不是请求认证，来源身份不能由未认证请求自行指定。
- normalize 强制只有 File 来源可以声明 global；Admin/Kubernetes 来源不能通过全局块控制监听器、预算、日志或 CA。已有名称空间站点与上游贡献保留，文件全局与多个运行时站点可共存。未据此宣称租户隔离：域名、指令、Secret/文件路径与上游访问仍需要后续能力策略。
- 新增六项测试：精确注册/原子拒绝、空注册、CLI 文件宿主拒绝 runtime 来源，以及三项规范化测试覆盖非文件全局拒绝、多来源站点共存、全局不被遮蔽。Windows 全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy、xtask、rustfmt/diff 通过；Linux plan 的 100 项单测与 72 项集成测试、CLI 单测及进程集成测试通过。未执行 Linux 全工作区、macOS、远端 CI 或长稳。
- 尚未完成 Admin HTTP 协议、认证/授权、来源能力策略、持久化恢复或管理 listener；此批不将 CTL-01/02 标记全部完成。

第九十一批（2026-09-07，CTL-01 / WEB-04 自动身份归属与代理严格解析）：

- 新增 ResourceId::derived_owned，隐式站点、路由和内联上游均按来源命名空间生成 ID；Admin 使用 runtime/，Kubernetes 使用 k8s/<namespace>/，File 保持原根 ID。is_derived 支持命名空间内的自动名称。不同命名空间不再生成相同根 ID，同站点相同内联后端仍复用上游，地址声明顺序不改变 ID。
- 非文件隐式资源 ID/状态键是有意变更，不承诺跨新旧身份迁移状态；anonymous_upstream_id 新增显式 owner 参数。此为逻辑身份归属，不替代 Secret、文件、域名或目标访问的能力隔离。
- 新测试揭示 proxy 简写会静默忽略第二个地址，现明确拒绝并提示 endpoint 子节点；未知属性和重复 upstream 属性同样在三种 proxy 入口拒绝，不再忽略配置。代理解析拆入 normalize/proxy.rs，主规范化模块回到文件规模目标内。
- 新增四项测试，覆盖模型命名空间、隐式资源/终端引用、站点内复用与地址顺序、简写参数/属性严格性。Windows 全工作区 all-features/all-targets/locked 回归通过；末尾模块拆分后 plan 100 项单测、75 项集成测试、严格全工作区 Clippy、xtask、rustfmt/diff 通过。初版多地址测试使用了错误简写，已改为合法 endpoint 形式，并为错误简写新增拒绝断言。Linux/macOS、远端 CI 和长稳本批未执行。
- Admin API、持久化恢复及完整来源能力策略仍待完成，不以本批身份修正宣称多租户隔离完成。

第九十二批（2026-09-07，RT-05 / WEB-04 内联上游策略隔离）：

- 修复同站点内地址相同的内联上游按地址去重、静默采用先声明策略的问题。最终匿名身份现根据完整解码配置派生，包含超时、pool、重试、健康、权重、balance、协议、TLS 与 resolver；先完成默认值解码，再决定是否共享。等价简写/块形式仍共享，不同策略独立，地址顺序仍无影响。
- 匿名上游 ID/状态键有意改变；策略变化会创建新的匿名状态，需稳定逻辑身份时使用命名 upstream 与既有 binding 规则。地址 helper 仅作为解码 seed，不再代表最终可复用运行身份。
- 新增三项编译测试及一项真实 HTTP 测试：反转路由声明仍各自采用正确 timeout/pool，等价默认值复用，权重/balance 分离；真实同后端 fast/slow 路由分别使用自己的响应期限且连接正常回收。初版 pool 夹具遗漏配套 idle 上限而被正确拒绝，补齐合法参数后通过，未放宽生产校验。
- Windows 全工作区 all-features/all-targets/locked 回归通过；末尾 HTTP 测试另行通过，严格全工作区 Clippy、xtask、rustfmt/diff 通过。plan 为 100 项单测、78 项集成测试。Linux/macOS、远端 CI 与长稳本批未执行。
- Admin API、持久化恢复和完整来源能力策略仍待继续完成，本批修复不代表全部扩展目标达成。

第九十三批（2026-09-07，RT-05 / CTL-01 路由状态身份唯一性）：

- 增加全配置站点 ID 与路由 ID 唯一性检查，不允许不同站点或不同路径的同名 route 静默别名到相同状态槽；重复诊断关联两处来源。资源种类、合法来源命名空间仍是独立身份域。
- 修复隐式 route ID 未包含额外 matcher 的问题。条件路由与 fallback 现在获得不同状态身份；matcher 的身份贡献与声明顺序无关，无附加 matcher 的旧 ID 保持不变。条件路由状态键有意改变，需要匹配条件变更时维持逻辑身份应显式指定唯一 ID。
- 新增三项编译测试与一项真实 HTTP 测试，覆盖跨站点/站点内重复 ID、条件身份/重排、种类/命名空间，以及普通路由耗尽限流额度后灰度路由仍可独立放行；NoOp 不重置两者计数。原重复请求测试继续断言原有歧义诊断，但不假设它必须排在新增身份诊断之前。
- 测试文件超过既有规模门禁后将路由匹配测试拆入 support/route_matching.rs，未放宽门禁。Windows 全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy、xtask、rustfmt/diff 通过；Linux model 42 项单测、plan 100 项单测/81 项集成测试通过，同时补验第九十一/九十二批编译变化。本批未运行 Linux 全工作区、macOS、远端 CI 或长稳。
- Admin API、持久化和完整来源能力策略仍待完成；本批不把身份唯一性等同于完整隔离。

第九十四批（2026-09-07，WEB-04 / RT-01 上游结构严格校验）：

- 新增共享 upstream/shape 校验层，在值解码前拒绝重复单例策略块，覆盖 balance/timeouts/pool/retry/health/passive-health/protocol/resolve/tls；endpoint 列表仍可重复声明不同后端。命名与内联形式、直接模型生产者共用同一规则，关联重复声明来源，不再默默使用第一项。
- 手写 balance/protocol/tls 解码器补齐参数数量、子节点、重复属性约束；protocol 不接受属性，错误类型的 TLS server-name 不再回退到后端名称。命名 upstream 必须恰好一个名称，顶层策略属性不能代替子节点。
- 新增四项矩阵测试，覆盖九种单例 × 两种入口、手写节点形状、模型绕过 KDL 的重复属性、合法多 endpoint；增强真实 HTTP 测试，重复 timeout 候选被拒绝并保留旧代及服务响应。初版合法夹具误用整数 duration，已改为标准字符串，生产校验未放宽。
- Windows 全工作区 all-features/all-targets/locked 回归通过；末尾增强 HTTP 测试另行通过，plan 为 100 项单测、85 项集成测试。严格全工作区 Clippy、xtask、rustfmt/diff 通过。Linux/macOS、远端 CI 和长稳本批未执行。
- Admin API、持久化和完整来源能力策略仍待继续实施；此次确定性配置修复不代表整体任务完成。

第九十五批（2026-09-07，RT-03 / RT-05 上游数值策略不再静默修正）：

- 删除 retry budget-percent 的 min(100)、主动健康阈值和被动 max-fails 的 max(1) 静默修正；超范围配置明确拒绝，避免表达与实际运行不一致。retry per-try、主动健康 interval/timeout 必须非零，HTTP 探测状态必须在 100..599，原有 timeout ≤ interval 约束保留。
- 0% 重试预算、1 的合法阈值及零 jitter 保持原值；未把所有零值一概视为非法，也未新增未经测量的最小正探测周期。非法值停在规范化阶段，不构造运行资源。
- 新增两项矩阵测试，覆盖九种非法边界与合法端点参数。Windows/Linux plan 100 项单测、87 项集成测试通过，Linux 同时补验第九十四批严格结构校验；Windows 全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy、xtask、rustfmt/diff 通过。Linux 全工作区、macOS、远端 CI 与长稳本批未执行。
- Admin API、来源能力策略及持久化恢复仍待继续实施，不将整体目标标记完成。

第九十六批（2026-09-07，RT-03 / RT-04 健康探测结构化取消）：

- 修复 probe 超时/调用方取消路径跳过 driver.abort 的问题：H1/H2 驱动由 abort-on-drop 守卫持有，所有离开路径均取消驱动，不再脱离探测保留连接。
- 探测轮次在请求进行中也监听 stop，停止后不报告该轮后端失败；probe/resolver 的 join_all 改为并发轮询自己持有的子 future，不再 spawn 后丢弃句柄。取消父轮次同步丢弃未结束子操作，结果保持输入顺序；子 panic 改为展开整个轮次而非静默漏报结果。
- Worker 停止超时后保留句柄执行 abort 并 join，不再因 timeout 消费句柄而脱离管理。仍属于协作式异步取消，不宣称可以强杀阻塞线程。
- 新增四项测试：H1/H2 × 超时/调用方取消均关闭真实 TCP 连接，父轮次取消释放全部子操作，并发完成顺序，以及 30 秒在途探测被 shutdown 提前取消且健康仍为 Unknown。Windows 全工作区 all-features/all-targets/locked 回归通过；末尾新增 shutdown 测试另行通过 Windows state 84 项测试，Linux state 87 项测试通过。严格全工作区 Clippy、xtask、rustfmt/diff 通过。
- 跨地址整体探测预算、worker 失败恢复、Supervisor 终止状态与更完整的 TLS 探测策略仍需继续审查；Admin API/持久化等整体目标未完成。本批未运行 macOS、远端 CI 或长稳。

第九十七批（2026-09-07，RT-03 / RT-04 Supervisor 终止与任务所有权）：

- Supervisor 增加完整生命周期锁，串行化 reconcile 与 shutdown；shutdown 置终止状态，之后 reconcile 不再启动 census/resolver/probe。重复 shutdown 仍可继续清理此前被调用方取消的停止过程；不宣称取消掉的 shutdown future 已经完成 joined cleanup。
- 探测和解析器复用条件增加 JoinHandle::is_finished 检查，已退出 worker 在下一次 reconcile 替换，running/resolving 不再列出已完成句柄。此为按 reconcile 恢复，不是带退避的自主重启循环。
- Worker 和 census Task 增加 Drop 取消守卫；Census 停止超时保留句柄 abort/join，与探测停止规则一致。独立使用 Census 的 follow/shutdown 仍可重启，Supervisor 外层负责终止边界。
- 新增三项测试：退出 worker 状态与替换、两种 reconcile/shutdown 竞态顺序及关闭后再调用、丢弃 worker 所有权触发取消。Windows 全工作区 all-features/all-targets/locked 回归通过，state 为 87 项；Linux state 90 项通过。严格全工作区 Clippy、xtask、rustfmt/diff 通过。测试初版错误地消费了交给 Supervisor 的句柄结果，修正为只等待 is_finished 后验证正常回收。
- 跨地址探测总期限、TLS 探测与自主失败恢复仍待完善；Admin API、持久化等总目标未完成。macOS、远端 CI 和长稳本批未执行。

第九十八批（2026-09-07，RT-03 多地址健康探测总预算）：

- probe_any 按逻辑 endpoint 记录单调起始时间，每个地址只获得剩余 timeout；预算耗尽后不再连接下一地址，不再把配置超时乘以解析地址数。快速失败且仍有余量时保留顺序 fallback，当前探测的取消/driver 回收契约不变。
- 新增三项真实网络测试：首地址耗尽后第二监听器未收到连接、503 地址后可回退到 200 地址、零预算不启动 TCP。未引入并行探测，也不保证每个地址都有独立时间片。
- Windows state 90 项测试、严格全工作区 Clippy、xtask、rustfmt/diff 及全工作区 all-features/all-targets/locked 回归通过。Linux/macOS、远端 CI 和长稳本批未执行。
- TLS 探测、自主 worker 恢复，以及 Admin API、持久化等剩余任务仍需继续完成，不缩减整体目标。

第九十九批（2026-09-07，RT-03 DNS worker 结构化取消）：

- 解析循环对整轮工作和刷新间隔都优先响应 stop；已停止或发送端已关闭时不再开始新一轮解析。关闭不必等待配置的 DNS timeout，也不把主动取消记为解析失败。
- 分离私有调度循环与实际系统解析，使用确定性悬挂 round 验证取消释放、发送端关闭、启动前已停止和刷新间隔中关闭；不依赖外部 DNS 恰好缓慢。
- 系统 resolver 的阻塞查询一旦执行，取消其异步等待不保证终止底层操作系统调用；当前实现不宣称具备强制 DNS 线程终止能力。
- Windows state 93 项、严格全工作区 Clippy、xtask、rustfmt/diff 及全工作区 all-features/all-targets/locked 回归通过（默认 ignored 专项未执行）。Linux/macOS、远端 CI 和长稳本批未执行。

第一百批（2026-09-08，RT-03 TLS 主动健康探测）：

- HTTP 探测复用业务池准备好的 ClientTls（CA/SNI/客户端身份/ALPN），但保持独立连接；TLS 握手计入 endpoint 总预算。TCP 探测仍保持 connect-only 语义。
- TLS 配置加入 probe worker 身份，reload 改变 TLS 策略时替换 worker；HTTP/2 显式携带与传输一致的 scheme 和配置的 health authority。
- 真实 TLS 服务矩阵覆盖 H1/H2、错误域名、错误 CA、恢复配置后重新健康；静默 TCP 服务验证 TLS ClientHello、握手超时关闭与 TCP 检查不发送握手数据。Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过；state 93 项，TLS 专项 2 项（含矩阵），默认 ignored 专项未执行。
- Unix TLS 路径已接入相同包装，但本批未执行 Linux/macOS；mTLS 专项、同路径凭据更新/紧急撤销、自主 worker 恢复、Admin API 和持久化等仍待完善。

第一百零一批（2026-09-08，RT-03 mTLS 与 Unix HTTP authority）：

- 新增真实 mTLS 服务矩阵，H1/H2 分别验证缺少身份、正确身份、不受信任身份、恢复与移除身份的 reload；服务端强制认证并核对客户端证书，健康/不健康交替避免旧状态误判通过。
- 修复 Unix 探测把文件路径作为 HTTP authority 的问题，默认 localhost，保留 health host 覆盖；新增真实 Unix socket 的 H1/H2 × 默认/覆盖四种组合验证。
- Windows TLS 专项 3 项、全工作区 all-features/all-targets/locked 回归、严格全工作区 Clippy、xtask、rustfmt/diff 通过；Linux state 97 项及严格 all-targets Clippy 通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 同路径凭据更新/紧急撤销、自主 worker 恢复、Admin API、持久化与跨平台长稳仍未完成，不缩减总目标。

第一百零二批（2026-09-08，RT-03 后台服务自主恢复）：

- 新增私有统一 worker recovery 层并接入 probe/resolver，子 future 意外返回或 unwind panic 后按 1/2/4/8/16/30 秒封顶退避重启，运行满 60 秒重置退避。普通下游探测失败仍由原有健康循环处理，不触发服务重启。
- 不生成 detached 子任务；关闭信号、发送端关闭与父任务 abort 都取消当前子 future/退避。增加结构化 worker_restart_scheduled 事件，不记录 panic payload。
- 新增四项确定性测试覆盖 panic/意外退出、退避封顶和重置、三种取消方式的 Drop、启动前停止。Windows state 97 项、全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过；Linux state 101 项通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- running/resolving 表示仍受管的服务（含退避期），不是每一刻正在执行探测；panic=abort、进程崩溃不在恢复契约内。Census 自主恢复、重启指标/抖动、凭据撤销、Admin API、持久化和长稳等仍待继续完成。

第一百零三批（2026-09-08，RT-03 Census 恢复与生命周期）：

- Census 后台周期采样接入统一 recovery，意外退出/panic 后按现有封顶退避恢复；保留 follow 时首次同步发布，每次后台重启后按原 5 秒间隔采样。
- 新增三项测试：真实采样闭包首次 panic 后恢复并持续采样、关闭/控制通道关闭不等待 tick、被 abort 的 Census 状态和 follow 重建，以及独立 Census 的 shutdown/follow 可重启语义。
- Windows state 100 项、全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过；Linux state 104 项通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。同步 publish 的 panic 和同步采样中途抢占不在异步恢复契约内。
- 重启指标/抖动、凭据更新与撤销、Admin API、持久化及跨平台长稳仍待继续完成。

第一百零四批（2026-09-08，RT-03 后台恢复指标）：

- Telemetry 新增固定 WorkerKind 与进程级计量，统一 recovery 记录 starts、意外 exits（panic/returned）和 backoff；metrics 终端直接输出三种服务共 12 条固定序列，无上游名称标签。
- backoff 使用 Drop 守卫，在正常等待结束、关闭和 abort 下释放；启动计数在工厂实际开始执行时增加，取消退避不会虚报重启。
- 新增固定序列/渲染与退避取消计量测试，并扩展真实 HTTP metrics 路由测试。Windows observe 54 项、state 101 项、全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过；Linux observe 54 项、state 105 项通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 指标独立原子更新，不承诺跨字段事务快照；重启抖动、凭据撤销、Admin API、持久化、跨平台长稳等仍待完善。

第一百零五批（2026-09-08，RT-03 App 激活顺序与终止边界）：

- App 为完整 activate/shutdown 增加生命周期串行化，取得权限后才读取当前提交，避免排队旧调用携带旧快照回写后台服务；关闭后 activate 不再执行，重复 shutdown 仍可完成取消后的清理。
- 在异步服务协调后重新检查当前提交，被新提交超越时先循环跟进，再执行指标、状态和证书清理。不声称同步 commit 与全部运行时副作用成为原子事务。
- 新增真实 App 测试：激活排队时更新配置，释放后只启动新 upstream；两种 activate/shutdown 顺序及关闭后再次激活均不能重启服务。Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过；Windows/Linux CLI 专项均通过（单元 11 项及各平台集成套件）。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 独立 Supervisor 调用者仍须按当前提交顺序传入 runtime；凭据撤销、Admin API、持久化、跨平台长稳等仍待完善。

第一百零六批（2026-09-08，SEC-02 已加载 TLS 凭据身份）：

- ClientTls 为验证后的 CA DER 与客户端证书链建立运行时身份，包含策略/协议并对字段分域，不哈希或展示私钥。准备阶段池兼容性结合该身份，probe worker 同步纳入实际池 TLS 身份。
- 修复已经进入 prepare 的 reload 仍因路径不变复用旧 TLS 池的问题；实际证书更换替换池与探测，PEM 空白变化保持复用。
- 新增真实 H1/H2 × CA/客户端身份同路径更新矩阵，验证有效→等价格式→不受信任→恢复、池 Arc 是否复用与健康状态变化。Windows/Linux TLS 专项各 4 项通过；Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 同 plan 的 NoOp 仍不观察外部文件内容，本批不宣称实现自动文件轮换；旧代/在途连接仍可持有旧池，不等于紧急撤销。外部依赖观测、撤销、Admin API、持久化等继续保留为待办。

第一百零七批（2026-09-08，SEC-02 显式外部资源刷新事务）：

- GenerationStore 新增 refresh/refresh_if，仅绕过同 plan 的 NoOp，保留来源身份、版本前置条件、准备失败回退、提交守卫与 head/generation 过期检查；成功发布新运行代但不伪造配置修订号。
- 新增五项事务测试：相同输入刷新、失败保留与重试、刷新被刷新超越、版本冲突、旧来源/同修订异内容拒绝。Windows plan 单元 105 项及集成套件通过。
- 扩展 H1/H2 × CA/客户端证书矩阵，同时验证普通配置变化 reload 和完全相同配置的显式 refresh；普通 attempt 仍为 NoOp，显式刷新替换实际变化的 TLS 池与探测。Windows/Linux TLS 专项各 4 项及 plan 单元 105 项、集成套件通过；Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- API 已具备资源刷新事务能力，CLI/文件观察者尚未自动调用；跨文件原子凭据快照、紧急撤销、Admin API、持久化与长稳仍待完善。

第一百零八批（2026-09-08，SEC-02 上游 TLS 文件自动刷新）：

- App.attempt 比较当前上游已加载 TLS 身份与文件中有效材料，变化/无效/缺失时调用 refresh_if，无变化保持 attempt_if/NoOp；资源读取前捕获版本，期间提交变化返回 Stale，不把旧观察附加到新代。
- 现有 watcher 事件及 5 秒 fallback poll 自动经过该路径，外部目录文件由轮询覆盖；无需为了凭据轮换修改配置文本或修订号。当前覆盖上游 CA/客户端证书和私钥，不涵盖所有插件秘密或服务端证书来源。
- 新增真实 reload_once 测试：CA/客户端身份同路径替换、同修订新运行代、无变化 NoOp、证书/密钥半替换、CA 损坏/缺失均保留有效旧代。Windows/Linux CLI 单元各 12 项及平台集成套件通过；Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff、fuzz locked/offline 元数据检查通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 不保证跨文件原子快照，不强制结束旧代连接；紧急撤销、其他外部依赖类型、Admin API、持久化与长稳仍待完善。

第一百零九批（2026-09-08，SEC-02 插件秘密依赖观察）：

- BuildContext.secret 接入同代共享 SecretDependencies，记录实际解析值的私有摘要，不进入计划/Debug/指标；限制每代 4096 个不同引用、重复去重、同次准备同引用异值拒绝。
- PreparedPlugins/Runtime 持有依赖，App 把依赖变化接入已有条件刷新，无需在 App 硬编码 basic-auth 用户结构。升级版本耦合 SDK 上下文构造，更新内置插件测试夹具。
- 新增容量、摘要脱敏、重复读取一致性测试，以及真实 reload_once/HTTP basic-auth 哈希文件轮换、旧密码失效、新密码有效、损坏文件保留旧代测试。Windows/Linux plugin-runtime 各 18 项、CLI 单元各 13 项及平台集成通过；Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 自行读文件的插件尚未覆盖；跨文件原子更新、在途请求撤销、Admin API、持久化与长稳仍待完善。

第一百一十批（2026-09-08，SEC-02 秘密读取与依赖封存）：

- 文件秘密改为检查实际打开句柄的类型/权限/长度，实际读取最多 64 KiB + 1 字节，不再仅靠读取前路径 metadata 控制内存；环境变量值也拒绝超出 64 KiB。无效 UTF-8 错误不回显内容。
- PreparedPlugins 完成后封存 SecretDependencies；后续 read 在 I/O 前拒绝，changed 观察仍可工作，防止运行期继续修改准备期依赖集合。
- 新增无限读取源上限、精确大小边界、UTF-8 脱敏、封存幂等/观察测试，并在真实认证轮换测试中验证运行代已封存。Windows/Linux model 各 44 项、plugin-runtime 各 19 项及 CLI 测试通过；Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、长稳与默认 ignored 专项本批未执行。
- 不宣称符号链接路径隔离、恶意文件系统读取超时或跨文件原子快照；紧急撤销、Admin API、持久化与长稳仍待完善。

第一百一十一批（2026-09-08，运行状态候选隔离与发布）：

- StateRegistry 增加独立候选注册表和一次性发布对象；兼容状态保留 Arc，替换/新增只在成功提交后进入活动注册表。被替换值延后释放，不在发布 guard 中执行插件析构。
- HTTP prepare 全链路使用候选注册表，发布前清除计划不再声明的条目；复用池的容量/解析策略延迟到成功提交后更新，并在释放注册表 guard 前完成，避免较旧发布覆盖较新策略。
- 新增三项注册表单元测试和三项 HTTP 集成测试，覆盖放弃发布、析构边界、失败候选保留旧状态与后续复用、持有真实连接时失败扩容不生效/成功扩容生效，以及过期候选不向获胜运行代泄漏状态。
- 验证通过：Windows 全工作区 all-features/all-targets/locked 回归、严格 Clippy、xtask、rustfmt/diff；Linux HTTP/状态 all-features 回归（状态 108 项，Windows 104 项）。最终发布顺序调整后，两平台状态发布/预算发布共五项集成再次通过。macOS、远端 CI、长稳和默认 ignored 专项未执行。
- 这不是复用状态的深拷贝或插件沙箱，也不承诺所有池指标/在途请求的原子快照；证书存储准备副作用、紧急撤销、Admin API、持久化与长稳仍待完善。

第一百一十二批（2026-09-08，证书候选增量与发布隔离）：

- HTTP prepare 使用独立 CertificateUpdates，手工证书、内部签发与 ACME 磁盘缓存恢复不再提前替换活动证书。成功提交后释放证书发布 guard，解析器继续引用活动存储，续期仍无需生成配置代。
- 使用增量而非全表快照，保留未修改站点的并发续期；内部/缓存证书只填空位，发布时再次检查，避免较旧缓存覆盖刚完成的签发。
- 新增放弃更新/guard、并发续期/重复发布单元测试，以及两项真实 HTTPS 握手回归：服务端新证书成功解析后客户端 CA 加载失败，原证书与响应保持不变；成功重载才切换；已被新提交超越的候选随后读入另一份有效证书，也不能覆盖获胜证书。
- 验证通过：Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、rustfmt/diff；Linux HTTP/TLS all-features 回归（TLS 单元 61 项，Windows 59 项）。最后补充的过期候选 HTTPS 测试在两平台定向重跑通过。macOS、远端 CI、长稳和默认 ignored 专项未执行。
- 本批隔离内存证书发布；内部 CA/账户文件创建、证书来源切换所有权、紧急撤销、跨文件快照和长稳仍待完善。证书 loaded/issued 日志表示准备时完成解析/签发，并非配置提交成功事件。

第一百一十三批（2026-09-08，证书来源身份与续期写入围栏）：

- 候选证书声明来源/主机身份；manual 纳入文件引用，ACME 纳入账户配置。来源或主机变化时不再复用旧证书；兼容重载保留证书与续期令牌。
- ACME 订单开始时获取槽位令牌，内存发布时原子复核；来源切换、删除 TLS 站点和切走再切回均拒绝旧任务结果。完整 TLS 站点集合与配置提交一起发布，不等待异步清理才撤销归属。
- 新增来源切换/ABA/移除/重复发布、放弃候选不撤销旧令牌单元测试；HTTP 测试覆盖 manual→internal→兼容重载→相同 ID 换主机→manual→删除 TLS，并验证恢复手工证书后的真实握手。原有 ACME 获取/缓存/重启测试通过。
- 验证：Windows 首次全量出现 CLI 静态文件测试 5 秒读超时，未改测试/时限的单独重跑和第二次全工作区 all-features/all-targets/locked 均通过，根因未定。Linux HTTP/TLS 回归通过（TLS 63 项，Windows 61 项）；严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、长稳及默认 ignored 专项未执行。
- 不宣称取消已经发往 CA 的订单或阻止其磁盘缓存写入；内部 CA 目录/材料轮换身份、账户持久化竞态、紧急连接撤销和长稳仍待完善。直接 CertificateStore.put 为受信任宿主覆盖入口，不是异步续期入口。

第一百一十四批（2026-09-08，ACME 活动账户所有权与可取消激活）：

- App 生命周期单独追踪真正激活的 ACME manager，不再使用最近准备缓存决定关闭对象；失败候选不影响旧账户复用或关机归属，成功替换先关闭旧 manager。
- 首次 sweep 改为受拥有权管理的后台任务，激活不再等待 CA；准备 readiness 同步更新。相同 wanted 集合保留现有任务，避免兼容重载中断订单。
- 取消同时覆盖 sweep 和间隔；shutdown abort 后 join，Worker Drop 保证等待被取消时也不脱管，不再超时丢弃 JoinHandle。is_running 排除已结束任务。
- 新增真实 App 失败账户候选/账户复用/关机测试、阻塞 sweep 不阻塞激活/兼容重载保留任务/关闭回收测试。ACME 集成测试改为有界等待实际证书就绪，而非假定 activate 返回即签发完成；重启夹具显式停止 ACME。
- 验证通过：Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、rustfmt/diff；Linux CLI/TLS 回归及 ACME 八项集成通过。CLI 单元两平台各 14 项，TLS Windows 62 项/Linux 64 项；macOS、远端 CI、长稳和默认 ignored 专项未执行。
- 磁盘缓存原子持久化、同步 I/O/密码运算中途取消、CA 端已接收订单撤销、独立嵌入宿主的 Drop 生命周期和长稳仍待完善。

第一百一十五批（2026-09-08，ACME 身份文件原子替换）：

- 用单个版本化、有界 `.identity` 文件保存证书链与私钥；校验匹配后 create_new 同目录临时文件、写入/sync/rename，Unix 创建即 0600 并同步父目录。读取使用同一打开文件的完整记录，校验长度、尾随数据与证书/私钥匹配。
- 新格式缺失时可读取旧 `.crt`/`.key`，损坏时不回退掩盖故障，不删除既有旧文件。重启与到期判断均接入新记录；证书文件读取改为检查已打开句柄并限制实际读取量。
- 新增发布前故障保留旧值/清理临时文件、并发读写不混对、损坏长度/截断/尾随数据、损坏新记录不回退四项测试；现有 ACME 签发/重启集成验证新格式落盘后恢复。
- 验证：Windows 首次全量在 TLS 探针五秒总时限失败；未改测试/时限的 probe_tls 四项复测和第二次全工作区 all-features/all-targets/locked 均通过，首次超时根因未定。Linux HTTP/TLS 全套通过，TLS Windows 66 项/Linux 68 项；严格 Clippy、xtask、rustfmt/diff 通过。macOS、远端 CI、断电故障、长稳和默认 ignored 专项未执行。
- 不宣称任意文件系统断电耐久、跨进程顺序、旧订单磁盘写入围栏或账户库持久化已解决；rename 后目录同步失败属于耐久性不确定，进程崩溃仍可能留下权限受限的临时文件。Windows 使用继承 ACL，Unix 模式测试不代表 Windows ACL 隔离。

第一百一十六批（2026-09-08，ACME 缓存目标订单排序）：

- CertificateStore 共享按规范化缓存目标索引的可取消订单 gate，覆盖联系 CA、落盘和内存发布，阻止同进程多个 manager 的同目标订单相互超越。不同目标独立运行，空闲 gate 用 Weak 持有并清理。
- 等待结束后复核原令牌和续期必要性，签发完成后、落盘前再次复核令牌；已知失效的结果不再先写磁盘后拒绝内存发布。网络/文件 I/O 不持有配置发布锁。
- 新增同目标串行/不同目标并行/取消释放/注册表回收、等待期间权限撤销、过期订单不覆盖新身份文件三项测试，原有 ACME 签发/缓存/重启测试通过。
- 验证通过：Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、rustfmt/diff；Linux TLS 单元 71 项与 ACME 八项集成通过（Windows TLS 69 项）。macOS、远端 CI、跨进程/断电故障、长稳和默认 ignored 专项未执行。
- 不宣称跨进程锁或配置提交与 rename 原子化：落盘前检查之后仍可能发生归属切换。不同账户共享目录的缓存来源隔离、恶意目录链接变更与断电故障仍待完善。

第一百一十七批（2026-09-08，ACME 账户存储命名空间）：

- 账户库与身份缓存统一放入 `<base>/accounts-v1/<完整哈希>`，身份使用版本化域、精确配置的 CA URL 与邮箱；不同 CA/联系身份隔离，续期窗口或同意策略变化不迁移缓存。
- 未标注来源的旧根目录缓存保留但不自动导入；命名空间内部的新身份格式与旧成对文件仍按已有读取规则工作。这是有意的存储布局变化，升级可能触发重新签发，应评估 CA 可用性与限额。
- Unix 新命名空间创建即 0700，现有目录若组/其他用户可访问则拒绝。Windows 保持继承 ACL，不以 Unix 模式检查声称跨平台沙箱。
- 新增命名空间稳定性/CA 与邮箱隔离、实际缓存不串用/旧根目录不导入、Unix 目录权限拒绝测试；更新旧文件恢复夹具、真实 ACME 重启路径与 App 测试目录清理。
- 验证通过：Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、rustfmt/diff；Linux CLI/TLS 回归与 ACME 八项集成通过。TLS Windows 71 项/Linux 74 项，CLI 两平台各 14 项；macOS、远端 CI、跨进程/断电故障、长稳和默认 ignored 专项未执行。
- 人工复制缓存的密码学来源验证、账户库原子持久化、共享账户的跨进程协调、恶意父目录替换与断电故障仍待完善。

第一百一十八批（2026-09-08，ACME 跨进程账户锁与取消边界）：

- 同一账户命名空间使用持久 `account.lock` 的 OS 排他锁；非阻塞尝试配合异步等待，可取消且不遗留后台锁等待者。锁覆盖签发、身份落盘和内存发布，不支持锁的文件系统明确失败。
- 等待完成后重新检查配置归属，并优先复用其他进程刚落盘、尚未进入续期窗口的证书；独立 CertificateStore 也不会因此重复申请。不同账户不共用锁。
- 每次签发取得锁后构造新的底层 issuer，重新加载账户状态；公开低层 `Issuer::issue` 同样持锁，但不承担 manager 的缓存复用和发布职责。
- 私有账户存储适配器以外层 OS 锁替代底层 lease 文件/续租任务；写入和删除由持有锁的任务执行，正常取消签发后仍保锁至该次存储操作结束，避免取消路径留下续租任务或提前让后续订单写入。
- 新增真实子进程争锁/强杀释放、等待者取消、不同账户不互阻、独立管理器复用新缓存、等待期间撤销归属，以及存储调用取消后保锁测试。Linux TLS 79 项、Windows TLS 76 项通过；两平台 CLI 回归和 ACME 八项集成通过。
- Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、rustfmt/diff 验证通过。macOS、远端 CI、长稳、默认 ignored 专项及断电故障未执行。
- 边界：只协调遵守协议的进程，运行中不得删除锁文件；不保证 NFS/分布式一致性、账户多文件事务、配置提交与 rename 原子性或跨副本 HTTP-01 路由。shutdown 尚不 join 存储任务；阻塞文件系统、整个 runtime 销毁及账户崩溃恢复仍待专项完善。

第一百一十九批（2026-09-08，ACME 存储关闭协议）：

- issuer 持有跨订单的存储任务计数；操作注册与启动之间无取消点，完成、失败、panic 均归还计数。取消等待不会丢失仍在执行的写入。
- manager 先停止并等待续期 worker，再等待已开始的存储操作结束；reconcile 替换 worker 同样先排空旧操作。worker handle 保留至 join 完成，取消 shutdown 后可重新等待。
- App 在账户切换、禁用 ACME 与关闭期间保留旧账户所有权，直到其等待完成；避免先 take 再 await 导致取消后丢失旧账户。
- 独立使用 issuer 的宿主可先停止签发调用，再调用 `drain_storage`；不以 drain 冒充任意并发公开调用的准入关闭。
- 新增取消 shutdown/reconcile 重试、存储失败和 panic 后排空测试，扩展原有取消写入回归。Windows 全工作区 all-features/all-targets/locked 通过，随后新增纯测试的 TLS 78 项通过；Linux CLI 回归、HTTP 单元 65 项和 TLS 81 项通过。严格 Clippy、xtask 与格式/diff 检查通过。
- 文件系统永久阻塞仍可能延迟 shutdown；整个 runtime 销毁、账户多文件崩溃恢复、断电与 macOS/远端 CI 验收仍待完成。

第一百二十批（2026-09-08，ACME 账户原子成对记录）：

- 确认底层库保存顺序为注册 JSON → 私钥，第二次失败还会删除注册文件。新增私有协议适配：第一次仅暂存内存，第二次校验后一次发布版本化 `account-<hash>.key`，避免更新半途留下新注册信息/旧私钥。
- 记录整体有界，校验 magic、精确长度、JSON 结构与私钥解码；临时文件同步后 rename，Unix 额外同步目录。保存失败的库回滚仅移除暂存，保留最后完整记录；无暂存时删除任一组件即删除整条账户记录。
- 使用 certon 真实 save/get/delete API 验证完整更新、两次写入间丢失暂存、坏私钥回滚、损坏/截断/尾随数据拒绝以及旧分离文件不导入。exists/stat 反映新记录，未使用的账户枚举明确拒绝，不伪装通用事务 Storage。
- 旧账户分离文件保留；已有证书缓存不变，下次签发可能注册新账户。CLI 升级说明已记录。JSON 校验属于 TLS 账户存储职责，新增显式 serde_json 依赖及带说明的分层允许项。
- Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、格式/diff 通过；Linux CLI 与 TLS 回归通过，TLS Windows 79 项/Linux 82 项。断电、真实崩溃注入、macOS、远端 CI、长稳和默认 ignored 专项未执行。
- 剩余：CA 已注册而本地尚未发布时仍可能留下远端孤立账户；预注册私钥恢复、受控导入、文件系统故障与整个 runtime 销毁仍需继续完善，不能把本地原子记录视作与 CA 的分布式事务。

第一百二十一批（2026-09-08，ACME 预注册身份恢复）：

- 发起注册前在账户锁内保存 `registration-intent.key`，后续重新创建 issuer 仍复用该私钥。已有完整账户可为缺失的 intent 提供密钥；损坏文件和密钥冲突明确拒绝，不静默轮换。
- 核实 certon 0.2.3 的实际路径，使用 pre-registration callback 注入已持久化身份，而非没有用于该创建路径的 account_key_pem 字段。文件同步/发布失败时不进入注册请求。
- 根据 RFC 8555 §7.3.1，同一密钥可恢复已有账户 URL。新增实际 HTTP 丢失注册响应测试，验证重开 issuer 后请求 JWK 相同；单元测试覆盖响应未落盘、完整账户保存后的复用、冲突和损坏拒绝。夹具不验证 JWS 签名或真实 CA 行为。
- Windows 全工作区 all-features/all-targets/locked、严格 Clippy、xtask、格式/diff 通过；Linux CLI/TLS 回归及 ACME 九项集成通过。TLS Windows 80 项/Linux 83 项。macOS、远端 CI、真实断电、长稳与默认 ignored 专项未执行。
- intent 应与账户记录一起备份，不随站点证书续期轮换；受控账户换钥/导入、整个 runtime 销毁和不同文件系统断电保证仍待实施与验收。

第一百二十二批（2026-09-08，ACME 存储协议收口）：

- 私有存储适配器改为显式键允许集合：仅接受规范账户 JSON/KEY 组件与注册 intent。未知布局、非预期文件名、路径穿越、反斜杠、大小写账户前缀别名、替代数据流及超长键均在文件访问前拒绝，不再回退普通文件存储；枚举明确不支持。
- 统一 store/load/delete/exists/stat 的校验，回归检查错误返回且磁盘仅保留原有锁文件；覆盖正常邮箱、带标签用户名、默认账户路径等成对映射。
- 关闭 certon 默认的 distributed solver 包装。当前 Gatel HTTP-01 仅使用 Challenges 表，并未读取库的磁盘 token 副本；移除重复持久化不影响已实现挑战功能，也不宣称支持跨副本挑战共享。
- Windows TLS 81 项、Linux TLS 84 项与两平台 ACME 九项集成通过；严格 Clippy、xtask、格式/diff 通过。Windows 全工作区 all-features/all-targets/locked 回归通过，默认 ignored 专项未执行。
- 底层库账户协议扩展必须显式评审后接入；此适配层不是插件任意文件访问接口。账户运维/导入、断电及整 runtime 销毁、macOS/远端 CI、长稳仍待完善。

第一百二十三批（2026-09-08，TLS / RT 生命周期与 certon 0.3）：

- 升级 certon 至 0.3.0，保留 ring 与禁用默认 feature；按发布源码评审 Policy/CertManager、CertStore 与账户 Storage 的职责分离。
- 生产证书保存/恢复采用 certon 的 CertStore/CertificateResource；新增 AtomicCertStore 原子完整记录，保留 issuer 元数据、同命名空间旧缓存只读回退和删除 tombstone。实际 CertManager 注入/加载、名称校验、损坏不回退及删除回归覆盖该契约。
- 保留 Gatel 的配置资源身份、活动证书发布令牌与账户事务适配，不将域名缓存直接替代 generation 状态。集成决策、迁移和限制见 [CERTON.md](CERTON.md)。
- ACME worker 改为仅持有独立 Renewal 状态，解除与管理器的强引用环；覆盖 idle 和阻塞状态下最后管理器释放后的取消。显式 shutdown 仍负责等待和存储排空。
- Windows 全工作区 all-features/all-targets/locked 测试和严格 Clippy 通过；Linux CLI/TLS 回归通过（TLS 86 项），Linux HTTP ACME 九项集成通过；xtask 与 diff 检查通过。真实外部 CA、macOS、远端 CI、断电和默认 ignored 专项未执行。

第一百二十四批（2026-09-08，TLS 证书存储边界）：

- 复现 certon 0.3 `names_key()` 截断产生的 SAN 集合别名；原子存储现在在读写两端拒绝超过 1024 字节的逗号拼接名称集合，避免将截断键再散列误认为能恢复唯一性。现有单域名签发不受影响。
- 新增不同有效证书共享截断键、旧格式资源绕过写校验、恰好 1024 字节可用、DNS/IP 多 SAN 与名称顺序无关、超大元数据更新保留原记录和临时文件清理检查。没有据此宣称多域名自动签发已开放。
- Windows TLS 86 项、HTTP ACME 九项、TLS 严格 Clippy、xtask、格式与 diff 检查通过。Linux TLS/HTTP 的 all-features/all-targets/locked 回归通过，其中 TLS 89 项；默认 ignored 专项未执行。外部 CA、macOS 与断电验收仍待完成。

第一百二十五批（2026-09-08，WEB-02 / QA-03 压缩互操作）：

- 对照旧 main 解压实现时发现 v2 `encode deflate` 沿用了裸 DEFLATE；按 RFC 9110 §8.4.1.2 改为 RFC 1950 zlib 封装，依赖 feature 从 raw deflate 改为 zlib，encode 行为版本升至 5。
- 先用封装头回归复现失败，再修复；新增 CM/窗口/FCHECK/字典标志、独立计算 Adler-32 与完整解压验证。真实 HTTP 覆盖 cache 配置下 identity/deflate 交替请求和重复响应，不再只检查 Content-Encoding 文本。
- Windows 全工作区 all-features/all-targets/locked 测试、严格 Clippy、xtask、格式与 diff 检查通过。Linux 定向验证标准 bundle 283 项、HTTP lib 65 项、directives 20 项通过。不宣称请求解压、浏览器或外部客户端互操作已完成；默认 ignored 专项未执行。

第一百二十六批（2026-09-08，WEB-02 / EXT-03 可选请求解压）：

- 新增独立 gatel-decode 和默认关闭的宿主 decode feature；gzip/br/zstd/标准 deflate、流式输入与受限输出缓冲、完整检查后转发、双大小/倍率/超时/并发限制。失败不先发送部分请求，明确不做无界 streaming upload。
- SDK 新增 TransformRequestBody 排序效果和 RequestBodyLimit 最小限额传递；解压不能绕过宿主或 body-limit 的输出限额。禁止同链重复 decode。许可随 Bytes 副本存活，准入状态按宿主身份/兼容性复用，Zstd 工作窗口单独有界。
- 四编码真实 H1 定长/chunked、H2 代理和后端未收到拒绝请求的回归已通过；覆盖容量、倍率、损坏、尾部垃圾、多成员、超时/取消、Bytes 生命周期、feature on/off 与默认依赖树裁剪。使用说明和迁移边界见 [DECODE.md](DECODE.md)。
- Windows 全工作区 all-features/all-targets/locked、严格 Clippy、feature 开关、最终新增 reload 专项、xtask/格式/diff 通过。Linux 首轮 CLI 两项生命周期超时；未改阈值，独立串行五项通过，随后原并行命令 gatel-decode/gatel-http/gatel 全目标全 feature 重跑通过（含新解压八项单测及两项集成）。首轮超时原因未确认；默认 ignored 专项未执行。外部独立解码器、长稳/fuzz、跨不兼容 reload 的全局资源预算尚未完成。

第一百二十七批（2026-09-08，WEB-02 / QA-02 解压输入与取消）：

- 补齐只在输出端 yield 的缺口：解码器可能吞入许多空成员而不产生输出。输入现在按最多 8KiB 分片并主动让出执行权；回归保证全 ready 的空成员输入不会一 poll 跑到 EOF，取消立即释放源 Bytes。
- 新增 Python gzip/zlib 独立固定样本、CRC/Adler 损坏、四编码逐字节分帧、完整成员之后的 typed body-limit 错误/连接重置/trailer 检查。普通解码器接受的 16MiB-window Zstd fixture 被受限后端拒绝，后续成员同样不能绕过窗口策略。
- Windows 解压 12 项、H1/H2/reload 两项集成、插件严格 Clippy、xtask/格式/diff 通过；Linux 解压 12 项、HTTP lib 65 项与两项集成通过。未宣称独立网络客户端、持续 fuzz/长稳或性能门槛已完成。

第一百二十八批（2026-09-08，QA-04 请求解压 fuzz）：

- 新增 request_decode coverage-guided 入口，直接编译生产 decoder 源模块，无新增生产公开测试接口。比较整块/分帧输入的结果、输出上限与未损坏生成流的精确还原；四编码、原始/生成/变异输入复用同一个 harness。
- 稳定测试执行 320 组固定/确定性生成输入；插件 12 项单测与该专项通过，Windows fuzz 目标 locked 编译、插件严格 Clippy、xtask/格式/diff 通过。CI fuzz matrix 已加入入口，但未执行远端 CI。
- 复用已有 Linux nightly 环境，默认 sanitizer 短测 31 秒、25,757 次、exit 0；从空语料开始，末尾长度探索上限仅 11 字节，不等于覆盖配置的 4097 字节上限。语料保留在本地任务容器，细节见 fuzz/README.md。
- 本批不包含独立网络互操作、长稳、长时 fuzz 或性能验收，也不以此宣称 WEB-02 / QA-04 整体完成。

第一百二十九批（2026-09-08，OPS-01 预压缩 CLI）：

- 恢复 `gatel precompress ROOT`：gzip/br/zstd sidecar、旧主要参数与别名、筛选、dry-run、实际读取容量限制、可重复更新；接入同一 Clap 命令树与生成手册/补全。未创建旧独立 gatel-precompress 二进制。
- 有界遍历跳过隐藏项、链接/reparse point 和已有压缩产物；独占临时文件、压缩错误传播、sync+rename、源元数据复检、失败旧产物保留。明确单 variant 原子性与离线受信树要求，不宣称跨文件事务或恶意路径竞态防护。
- Windows CLI 四项、生成文档三项、提交前故障单测通过；实际宿主能协商并返回三种预压缩产物。宿主严格 Clippy 通过；Linux 宿主全功能全目标测试通过，含五项预压缩集成及 Unix 权限/链接检查。Windows reparse point 专项、恶意目录竞态、断电和整个树的事务部署仍未验收。使用与迁移说明见 [PRECOMPRESS.md](PRECOMPRESS.md)。

第一百三十批（2026-09-08，WEB / QA-02 静态预压缩新鲜度）：

- file-server 在编码协商前按源文件 mtime 排除过期 sidecar，未知时间退回 identity；允许相同时间戳。行为版本从 9 升为 10，避免配置指纹将新旧行为视为相同。
- 新增真实 HTTP 回归，覆盖 br/zstd/gzip 同时间戳、源更新后 GET/HEAD 不选旧压缩文件及旧 ETag 不误返回 304、过期高优先级格式不阻挡新鲜低优先级格式。使用显式文件时间，无 sleep。
- Windows：标准 bundle 281 项、file_variants 3 项、其余静态文件与 directives 24 项、真实 precompress CLI 4 项测试通过；bundle/http all-targets 严格 clippy 通过。本批未执行 Linux/macOS；没有宣称时间戳检查等同于内容验证或跨文件并发快照，限制见 PRECOMPRESS.md。

第一百三十一批（2026-09-08，WEB / QA-02 编码协商契约）：

- 确认旧 Option 返回值混淆 identity、全部不可接受及语法错误；内部协商改为 Coding/Identity/NotAcceptable/Invalid 显式结果，两个消费者统一接入。
- 新增拒绝、通配符、跨字段重复、identity 优先级、空可用集合与畸形输入测试。Windows bundle 282 项、directives 20 项、file_variants 3 项通过。
- 当前消费者仍只取 Coding，保留原响应行为，尚未实现最终 406。PRECOMPRESS.md 明确后续表示决策、动态压缩、HEAD/条件/Range、错误页和缓存的组合验收要求；不能只在文件 handler 提前拒绝，否则会破坏外层动态编码机会。

第一百三十二批（2026-09-08，WEB / QA-02 文件最终表示选择）：

- encode 登记请求级编码能力；文件服务合并静态 sidecar 与动态编码，在条件请求前选择表示，无可接受表示返回显式空 406/no-store。修复测试发现的框架默认错误页填充与零长度头冲突。encode/file-server 行为版本分别升为 6/11。
- 动态文件编码的 GET/HEAD 共用弱 ETag 和编码，HEAD 不误报 identity 长度；编码 Range 返回完整表示。identity-only 文件也声明 Vary，防止暖缓存绕过编码拒绝。
- headers 提供内部响应操作预演，按实际 unwind 顺序应用，文件编码资格遵守配置的 no-transform/Content-Type。嵌套站点/路由覆盖、尺寸和类型限制、静态/动态质量优先级及 stale sidecar 回退有真实 HTTP 测试。
- Windows bundle 282 项、file_variants 6 项、directives 20 项、其他 file 测试 4 项、cache 23 项、cache_revalidate 2 项、precompress CLI 4 项通过；严格 bundle/http all-targets clippy 通过。本批未执行 Linux/macOS。非文件终端及任意插件后置策略仍未完成统一协商；自定义错误页等组合仍待验收，未将 WEB 或整体规划标为完成。

第一百三十三批（2026-09-08，WEB / QA-02 错误页表示替换）：

- 真实 HTTP 回归先复现自定义错误页覆盖空 406 后旧 Content-Length=0 导致 hyper panic、客户端无响应；修复 error-pages 的表示替换契约，清除旧编码、长度、范围、校验器、摘要和内容描述字段，设置新页面的准确字节长度。行为版本升为 2。
- 保留状态及协议控制头，包括 WWW-Authenticate、Retry-After、Allow、Set-Cookie、Cache-Control、Vary。单元测试逐项验证清除/保留；真实 GET/HEAD/重复 GET 验证 406 自定义页面、无原资源泄漏、正确长度和 no-store，既有默认不替换后端错误的测试保持通过。
- Windows bundle 283 项、file_variants 7 项、guards 9 项通过；严格 bundle/http all-targets clippy 通过。本批未执行 Linux/macOS；自定义说明页自身的完整编码协商仍未完成，不将当前修复扩称为所有响应变换的统一契约。

第一百三十四批（2026-09-08，QA-01 Linux 组合验收）：

- 完整同步当前 crates/plugins 和根依赖清单到隔离 Linux 容器，修复 Unix 专用 sidecar 链接测试仍调用旧 precompressed_variant 接口的编译失败；保留外链拒绝、内链可用的验证。
- Linux 标准 bundle all-features/all-targets 285 项、HTTP 核心 65 项通过。随后单独运行 file_variants/file_conditions/file_access/file_listing/directives/guards/cache/cache_revalidate 共 64 项通过，补足近期文件表示选择、HEAD/406、错误页长度与缓存组合的 Linux 证据。最终测试镜像 `gatel-codex-linux-verify:check`：`d0381f086d2213b31c1cce9d89f147e47db2a6faeb9280593cdd9b7b69c4dedf`。
- 扩大的 Linux HTTP all-targets 验收未通过：ACME 的 a_certificate_is_kept_across_a_restart、a_certificate_is_obtained_and_then_served、lost_registration_response_reuses_the_same_wire_identity_after_restart 超时。进程已退出 1，未据此重启或替换测试逻辑。
- Windows 宿主 all-features/all-targets 也未通过：failed_account_candidate_does_not_replace_active_shutdown_ownership 在 app_lifecycle_tests.rs 的 app.activate() 一秒期限超时；单项重跑同位置再次失败。未增加期限或跳过该测试，下一步需定位激活等待阶段与 Linux 本地 CA 失败原因，二者是否同源尚未证实。
- Linux cache 守卫测试首轮因没有已提交 generation 失败；补充 apply Outcome 的直接成功断言以保留根因诊断，随后同组重跑 23 项通过，Windows 单项也通过。该暂态原因未确认，不推断为端口或负载问题。完整工作区、ACME、macOS 和远端 CI 仍无本轮通过结论。

第一百三十五批（2026-09-08，QA-01 超时复查与诊断）：

- 检查 app.activate 的 listener/upstream/ACME 等待阶段；临时测试探针下单项通过，随后移除探针，未改生产激活逻辑或任何期限。Windows `cargo test -p gatel --all-features --all-targets --locked --quiet` 33 项通过，HTTP ACME 9 项也通过。
- ACME 注册丢包测试增加 JoinHandle 观察：若签发在 CA 接收注册前结束，立即报告实际结果；若已收到注册，继续原有两次重启密钥一致性验证。完成的 handle 不重复轮询，未完成的任务仍 abort/join，再排空 issuer 存储。没有延长原十秒期限。
- Linux 先单独通过注册丢包测试及完整 ACME 9 项，再运行 `cargo test -p gatel-bundle-std -p gatel-http --all-features --all-targets --locked --quiet`，最终退出 0。原有两项 ignored 未执行（含 soak），不能宣称长稳或完整 workspace 通过。镜像 `451a2f3eab113881f7b2e8577d2edff2d30608cdf034eeb72be2c395b435a0eb`。
- Windows HTTP all-targets 严格 clippy 通过。第 134 批超时本轮未复现，但尚无证据将根因归于负载、网络、账户锁或其他因素；保留失败历史，不将“同期限重跑通过”描述为根因已修复。后续重复/长稳验收仍需监测这些路径。

第一百三十六批（2026-09-08，QA-04 实际短时 soak）：

- 审计发现 soak 原先忽略 Range 结果和配置提交失败，也未启动配置中的健康探测。新增精确 206/Content-Range/body 断言、每次提交成功断言、真实 Supervisor reconcile、状态/指标回收及最终五秒监听器/工作器排空；未放宽原增长阈值。
- RSS 改读 Linux /proc/self/status 的 VmRSS，移除固定 4 KiB 页假设；修正文档中过强的短测无泄漏推断和过时 cache 元数据预算描述。新增 SOAK.md 说明串行客户端、进程内采样和发布级验收缺口。
- Linux 第一轮未接入 Supervisor 的 60 秒基线：8520 轮通过，RSS 17320→23860 KiB；该轮不算探测生命周期验收。随后运行最终加强版：60.03 秒、8163 轮、40815 次业务请求，通过所有计数检查及排空断言，退出 0。
- 加强版基线 state=4、metrics=187、sinks=1、idle=1、tasks=6、cached_resources=6；最终分别为 4/187/1/0/5/6。RSS 18104→24984 KiB 仅报告不做零泄漏结论。镜像 `86f21774012f91cc4885e55919f2e917e17a42a5221fd03dd70cd8ec04ba570c`，GATEL_SOAK_SECONDS=60，cargo test -p gatel-http --test soak --locked -- --ignored --nocapture。
- Windows soak target 严格 clippy、格式和架构检查通过；本批未执行 Windows soak。小时级、多协议、独立进程 FD/堆分析、持续身份变化和并发客户端仍待执行，QA-04 未完成。

第一百三十七批（2026-09-08，RT / QA-04 持续身份退休）：

- 新增默认执行的 120 轮身份 churn 回归：每轮生成新的 upstream/endpoint/site/route/log 身份，真实请求并协调 Supervisor；除计数外，用弱引用直接验证前 119 个 generation 与 cache store 失去全部强引用。
- 首轮稳定复现：generation 全部释放，但第 118 轮 cache 仍有一个强引用。原因是当前 Runtime 的 StateUpdate.retired 保留前一代注册值直到当前 runtime 退休。并非无限累积，但不必要地延迟缓存等插件状态回收。
- PublishRuntime 新增默认空的 finish_publication；宿主在 generation 锁和所有 publication guard 释放后调用，HTTP runtime 通过 StateUpdate.finish 取出并释放退休值。维持原子 swap 内不运行插件析构的边界；Arc 实现转发新阶段，finish 可重复调用。新增析构时注册表锁可取得的单元测试。
- Windows plan/state 共 297 项及 churn 回归通过；plan/state/http all-targets 严格 clippy 通过。Linux churn 与 state/certificate_publication 共 7 项通过，退出 0；镜像 `7ecc8c9674f163956cd6bbaf982f00642bf686fc0c3ecdf06b58879dfbc6a0e6`。本批未重跑计时 soak、全 workspace 或 macOS，长时间与全部插件析构行为仍需继续验收。

第一百三十八批（2026-09-08，TLS / RT 发布后回收契约）：

- CertificateUpdates 与状态发布存在相同的退休暂存持有。新增 finish，在宿主 finish_publication 中释放被替换/移除/未采用的证书材料；实际 TLS 读者的独立 Arc 不受影响，不宣称底层安全擦除。
- 新增证书弱引用测试，覆盖替换、删除、并发续期胜过 cached 候选和重复 finish。真实 HTTP 证书发布测试在下一次 reload 前直接验证退休 key 强引用归零，避免由再一次 reload 掩盖延迟释放。
- 新增 generation_finish.rs 契约测试，验证 finish 时 generation 写锁及 runtime 发布锁均可取得，并能调用 current/status；成功发布才调用，NoOp、PrepareFailed、Stale 不调用，Arc 包装正确转发。
- Windows TLS 87 项、发布后清理契约 1 项、HTTP certificate/state publication 与 identity churn 7 项通过；plan/tls/http all-targets 严格 clippy 通过。Linux 同组 HTTP 7 项通过，计时 soak 仍 ignored；镜像 `070b56778b9d11320a5bb4523a555736f6516c082c9e189e9d4b1ef69409a178`。本批不代表完整 workspace、小时级或外部 CA 验收。

第一百三十九批（2026-09-08，RT / QA-02 并发发布后清理）：

- 新增双线程门控测试：第一代已提交但 finish 被暂停时，第二代仍须在两秒内完成提交；释放旧 finish 后当前 generation 保持为第二代。通过通道同步而非 sleep 猜时序，断言失败前先释放门控；可见性检查使用 try_read，避免锁回归让测试自身永久卡死。
- 状态与证书各新增逆序 finish 回归：先发布两次、先清理第二次再清理第一次，通过弱引用证明仅释放各自退休值，最新值仍保持相同 Arc。明确 PublishRuntime.finish_publication 契约：执行时当前 generation 可能已更新，不能按当前 store 推断清理目标。
- Windows 新增 3 项回归与既有 finish 契约通过；plan/state/tls all-targets 严格 clippy 通过。Linux plan/state/tls lib 共 308 项通过；最终非阻塞可见性检查版并发用例单独复跑通过，镜像 `1d43d2a3756933f1f65ad34ee8611f09372e96be59feda5cd3098fd1f418fa20`。本批未改变生产行为，未执行全 workspace、soak 或 macOS；整体规划仍未完成。

第一百四十批（2026-09-08，QA-01 Windows 全工作区回归）：

- 执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，最终退出 0；汇总 92 个测试结果段，1253 项通过、4 项 ignored。覆盖本轮当前源树中宿主、可选插件及所有 workspace 成员，而非仅 TLS/HTTP 子集。
- 四项 ignored 分别是独立 FastCGI transport 的真实 PHP-FPM、HTTP FastCGI 的真实 PHP-FPM、计时 soak、PHP 密码互操作，本批未显式执行。`cargo test --workspace --all-features --doc --locked --quiet` 正常结束，但唯一 doctest 为 ignored，不能计为执行通过的示例。
- `cargo clippy --workspace --all-features --all-targets --locked -- -D warnings`、fmt、xtask、diff 检查通过。没有修改生产逻辑或放宽测试门槛；第 134 批超时在本轮完整 Windows 范围未再复现，根因仍未知。
- CI soak 命令补 `--locked`，并移除短时间压力测试可以排除几乎全部泄漏的过强说明。该工作流只在本地编辑，没有推送或触发远端执行。
- 本批不证明 Linux/macOS 全工作区、MSRV、最小/逐插件 feature 构建、供应链、小时级负载或发行验收完成；QA-01 和整体完善目标继续保留。

第一百四十一批（2026-09-08，QA-01 最低 Rust 版本实测）：

- 使用已安装的 Windows Rust 1.94.0（rustc 4a4ef493e、cargo 85eff7c80），实际执行 `cargo +1.94.0 check --workspace --all-features --all-targets --locked --quiet`，退出 0。验证包含测试目标的编译，不只是库和默认二进制。
- 同工具链执行 `cargo +1.94.0 test -p gatel-plan -p gatel-state -p gatel-tls --lib --locked --quiet`：107/106/88 项，共 301 项通过。本命令不是全 workspace 测试，也不包含显式 ignored、doctest 或全平台运行结论。
- CI 的 MSRV job 补 `--all-targets`，README Building 明确 Rust 1.94/edition 2024 和锁定依赖编译门禁；没有修改最低版本声明或依赖锁来回避兼容性问题。工作流尚未推送、远端执行，Linux/macOS 的 MSRV 仍需独立证据。

第一百四十二批（2026-09-08，EXT / QA-01 可选能力隔离）：

- Windows 分别执行 none、maintenance、fastcgi、scgi、cgi、decode、argon2 七种 no-default-features 宿主构建，各运行 plugins/passwd 两组真实 CLI 测试，共 57 项通过。覆盖能力发现、配置接受/拒绝、密码算法开关及生成 hash 的真实 HTTP basic-auth 验证；不是仅编译成功。
- 每个组合执行 cargo tree -e normal --prefix none --locked，逐项拒绝未选中的 gatel-maintenance/fastcgi/scgi/cgi/decode 包；除 argon2 组合外拒绝 argon2 包。七组生产依赖隔离检查均通过，不将 dev-dependency 树误当为部署依赖。
- CI standalone plugin matrix 补 CGI，互斥插件检查也包含 CGI，并禁止普通单插件宿主引入 Argon2。password-interop job 新增最小宿主和 Argon2-only 的 plugins/passwd 测试步骤。配置已修改但未推送或执行远端 CI。
- 本批为 Windows 单能力组合验收，不覆盖所有多插件排列、Linux/macOS 同矩阵、ABI 稳定性或第三方独立仓库插件。格式/架构/diff 检查通过，EXT 与 QA 任务仍保留未完成项。

第一百四十三批（2026-09-08，BASE / OPS 用户入口文档核对）：

- README 删除 CGI 家族未实现的过时说明，区分默认标准指令与源码链接的可选 CGI/FastCGI/SCGI、maintenance、decode、Argon2id；补齐 CLI 命令、插件包结构、操作文档和旧 main 迁移台账入口。
- 内部证书描述改为有条件的 TLS-only 本地站点推断或显式 `tls internal`，不再暗示 trust 本身会配置监听或保证所有客户端可访问。没有执行系统信任库修改。
- Building 补锁文件、选择性 feature 示例和完整 feature 测试命令，明确 ignored 测试需要单独执行。分层描述改为实际 allowlist，不把目录列表误写成依赖拓扑。
- Windows 实际执行 CLI `--help`、默认 `passwd --help`，以及 fastcgi,decode 组合的 check 和 capabilities；组合包含 std/fastcgi/decode，命令退出 0。fmt、xtask、diff 检查通过；未重新执行 release 构建或全 workspace 测试，本批不新增功能完成声明。

第一百四十四批（2026-09-08，WEB / OPS 预压缩时间戳一致性）：

- 真实 CLI → HTTP 回归先复现：源文件 mtime 比当前机器时间晚一小时，precompress 返回成功，但生成的三种 sidecar 均早于源文件，file-server 拒绝选择并返回 identity。测试在缺少 Content-Encoding 处失败，不是仅由静态审查推测。
- 临时压缩文件在 sync/rename 前继承源快照 mtime，使刚生成的产物通过新鲜度筛选；时间设置失败仍走 rename 前清理、保留旧产物。没有放宽服务端 freshness 检查，也没有承诺跨文件原子快照或恶意并发写防护。
- Windows precompress 集成 4 项通过，包含未来时间戳的真实 HTTP gzip/br/zstd、反复生成解压及 mtime 相等检查；源变化保留旧产物/清理临时文件的单元测试 1 项通过。默认宿主 all-targets 严格 clippy、fmt、xtask、diff 检查通过。本批尚未在 Linux/macOS 重跑，也未新增确定性取消注入覆盖。

第一百四十五批（2026-09-08，OPS / QA-01 预压缩 Linux 验证）：

- 将第 144 批预压缩修复及测试同步至已有隔离 Linux 容器，镜像快照 `df7d474e29aecc3750d6118f3a8fe63b6d8ce11528e2a47f4a7f3da9f028e3cc`。未修改其他服务容器。
- 新增 Unix 权限矩阵：0400/0444/0640/0755/06755 源文件，逐一验证 gzip/br/zstd 产物仅继承读写位、mtime 相等且源权限不变。与原有 0600、符号链接跳过/目标拒绝测试共同覆盖只读文件、执行位及 setuid/setgid 清除。
- Linux `cargo test -p gatel --locked --test precompress --quiet` 6 项通过，`--bin gatel precompress::tests` 1 项通过。随后使用同一已编译集成测试二进制，以 UID/GID 65534 运行，6 项再次通过，避免仅以 root 成功推断普通用户行为。
- 以上包含 CLI 生成、更新、解压、未来时间戳真实 HTTP 编码协商与失败保留旧产物；不代表全 workspace/Linux 完整验收。macOS、Windows reparse point、取消注入和其他文件系统/ACL 仍待独立验证。

第一百四十六批（2026-09-08，OPS / QA-01 Windows reparse-point 验收）：

- 新增实际 Windows 目录 junction 测试，夹具检查 FILE_ATTRIBUTE_REPARSE_POINT 确实存在，不以普通目录模拟、不因缺权限静默跳过。junction 创建不依赖文件 symlink 权限，路径包含空格。
- CLI 以 junction 为根明确退出 3；树内 junction 被跳过且目标目录没有新增压缩文件；sidecar 目标为 junction 时，正常执行和 dry-run 都退出 3，链接与外部临时哨兵内容保持不变。清理使用单个 remove_dir 移除链接，不递归操作 junction 目标。
- Windows precompress 集成测试共 5 项通过，默认宿主 all-targets 严格 clippy、fmt、xtask 与 diff 检查通过。本批增加回归覆盖而未改变生产路径策略；文件 symlink、其他 reparse tag、并发替换与 ACL 矩阵仍不宣称已验收。

第一百四十七批（2026-09-08，WEB 路径公开策略闭环）：

- Linux 真实 HTTP 测试先复现公开符号链接 `alias.txt` 指向根内 `.private/secret.txt` 时返回 200 和私有哨兵内容。原实现仅对 URL 名称应用隐藏规则，解析后的目标只检查根包含关系。
- 新增共享 `public_resolved`，对 canonical 路径相对于配置根的全部组件应用公开名称规则；用于直接文件/目录、索引及预压缩辅助文件、目录列表。配置根自身可位于隐藏目录下，不把用户明确指定的根祖先纳入公开名称检查。
- 回归覆盖隐藏文件/目录别名、索引别名、足够新鲜的压缩目标，以及正常内部链接继续工作。压缩目标刻意设置未来 mtime，避免测试仅因 freshness 检查拒绝而假通过。硬链接别名和并发路径替换不在此保证内；这是收紧既有隐藏策略，不新增恶意文件系统沙箱承诺。
- Windows 文件相关单元测试 42 项、file_access/file_listing/file_variants HTTP 测试 10 项通过；Linux 对应单元测试 44 项、HTTP 测试 10 项通过，最终加强压缩时间戳夹具后 file_listing 2 项再次通过。Windows bundle/http all-targets 严格 clippy、格式、xtask、diff 检查通过。Linux 最终镜像 `f3247ab0f192a7094665c61be5028679aef91feea546723888450242cffb4398`；未执行完整 workspace 或 macOS 验收。

第一百四十八批（2026-09-08，CGI 显式脚本选择边界）：

- 检查 CGI/FastCGI/SCGI 共用 Script 映射后，确认它与静态文件公开目录策略不同：脚本由可信配置固定，请求路径只产生 PATH_INFO。因此不将第 147 批隐藏路径规则机械应用到 CGI，不禁止部署者显式选择根内隐藏脚本。
- 新增共享层回归：本地/remote-root 下 SCRIPT_FILENAME 不随精确 URI、后缀、相似前缀、其他脚本名或 Unicode 路径改变；隐藏固定脚本被允许；缺失脚本、目录和 traversal 均拒绝，即使配置 remote-root 也不能省略本地验证。
- Unix 用实际 symlink 验证根外相似前缀目录和断链拒绝、根内固定隐藏脚本链接允许。远端映射保留配置脚本名称，不猜测远端链接布局。该测试验证解析与环境契约，不冒充真实 FPM/SCGI/子进程端到端验收。
- Windows gatel-cgi-http 全部库测试 24 项、Linux 25 项通过；Windows 对应 all-targets 严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `41ab1f0003626f1702ed3b8692e17839239e7ffcf5182179bdfc6ca18ee30b16`；没有改变生产映射语义。

第一百四十九批（2026-09-08，QA-01 Linux 全 workspace 回归）：

- 将当前 crates、plugins、xtask 和 Cargo.toml/lock 同步到隔离 Linux 快照 `af051cd99d3d8400d01be2345655b5b16a8a3bd73271023d94c1e6f2aceed76f`。执行环境 rustc 1.98.0（88d9e12ae）、Linux 6.18.33.2-microsoft-standard-WSL2；不是 MSRV 或 macOS 验证。
- 实际执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，退出 0；汇总完整输出 92 段 test result，共 1272 passed、0 failed、4 ignored。忽略项仍为两处真实 PHP-FPM 互操作、定时 soak 与 PHP 密码互操作；doctest 不在 all-targets 范围内。本轮不把这些项目计入通过数。
- 宿主生命周期、本地 ACME CA、插件、文件路径新规则等纳入本次组合回归。过去 ACME/宿主超时记录仍保留，不能因本轮通过就宣称历史根因已修复；没有扩大期限或删除失败断言。
- 预压缩 6 项测试在全量运行中最终通过但耗时 97.66 秒并触发 over-60-seconds 提示；随后同快照以 `cargo test -p gatel --all-features --locked --test precompress --quiet` 单独复跑，6 项通过、测试时间 0.43 秒。长延迟未稳定复现，根因未确定，不据此作性能达标或无退化声明。

第一百五十批（2026-09-08，CGI / QA-03 真实 PHP-FPM 与持续验收）：

- 使用本地缓存固定 PHP 镜像 `php@sha256:59fa733c9af643a122f8a9976119460e35ce76dd0a3f2b9c8f75af8e361a54e2`（8.4.25），创建独立后端并复制 examples 至 /app，仅读写本轮夹具，端口仅映射 localhost；没有更新镜像或修改其他服务。
- Windows 与 Linux 分别显式运行 `gatel-fastcgi --test exchange real_php_fpm_transport` 和 `gatel-http --test fastcgi real_php_fpm_get_post_and_head` 的 ignored 测试，均带 --locked，共 4 次通过。HTTP 覆盖 GET、非空 POST 上传回显及 HEAD 无正文；不计为 Unix socket、SCGI 应用或生产 PHP 环境验收。
- CI 新增 fastcgi-interop 作业：固定镜像、就绪检查、随机 localhost 端口、两组显式测试及 always 清理，普通测试继续忽略未知外部后端。工作流尚未推送/远端运行，不把配置存在计为 CI 通过。
- 已核对并停止/删除本轮 `gatel-php-interop-150` 容器，原镜像保留。测试命令全部退出 0，diff 检查通过；没有因补运行两个 ignored 项而宣称其余 soak/PHP 密码互操作已经验收。

第一百五十一批（2026-09-08，AUTH / QA-03 密码独立实现矩阵）：

- 先显式运行原 PHP ignored 互操作测试通过，再将单个 ASCII 密码扩展为首尾空格、中文/emoji/重音 UTF-8、71 字节 ASCII 三种固定夹具；bcrypt 与 Argon2id 各双向验证，总计 6 个组合。PHP 与 Gatel 均拒绝附加字符的错误密码，适用夹具还拒绝 trim 后输入。
- 新矩阵实际执行通过：1 个 Rust 测试、6 个双向组合，Windows Rust 与固定 Linux PHP 8.4.25；仍使用离线已有镜像、无网络、只读根、内存/进程限制和仅删除自身创建 ID 的清理机制。测试后检查没有该镜像的遗留容器。
- Windows gatel-password all-features/all-targets 严格 clippy、fmt、xtask、diff 检查通过。CI 原 password-interop 命令自动覆盖扩展矩阵，但没有远端执行证据；不将本轮扩大为 Linux Rust、所有 Unicode 规范化、256 字节 Argon2 上限或 bcrypt 旧 72 字节凭据的验收。

第一百五十二批（2026-09-08，WEB 条件缓存验证器歧义）：

- 真实 HTTP 回归先复现源站返回两个不同 ETag 时，fresh cache 只读取第一个值并自行返回 304，响应还带两个 ETag。测试先证明无条件请求实际命中缓存，避免只覆盖未存储响应的旁路。
- 条件缓存增加 ETag 单值与语法检查，复用 preconditions 标签语法；重复（包括相同值）或畸形 ETag 不再作为本地 304/正文判断证据，而是原样转发客户端 If-None-Match 交给源站。无条件缓存读取保持原行为，不声称所有源站元数据都已验证。
- 新真实源站用例覆盖不同重复、相同重复和非法标签，确认客户端条件头到达源站并返回其新正文；原有 weak/list/wildcard 条件命中仍由 cache 测试覆盖。
- Windows/Linux 均执行 cache/cache_race/cache_revalidate HTTP 测试，各 27 项通过；Windows preconditions 单元测试 2 项、bundle/http all-targets 严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `87bc97dffcc3fd3fe6dc8fa22b4aa511b9277af901e6c7beb5ea4f558c813d6a`。本轮不是全 workspace 重跑，也不代表完整条件缓存 RFC 功能已完成。

第一百五十三批（2026-09-08，WEB 条件回源替换后的复用）：

- 扩展第 152 批歧义 ETag 真实源站测试，使回源 200 明确可缓存；源站任务退出并关闭 listener 后，再断言普通请求命中新正文、对新 ETag 的请求从缓存返回无正文 304。三个歧义夹具都执行该后续序列，避免只证明一次回源成功却未检查旧条目是否重新出现。
- Windows/Linux cache_revalidate 各 3 项通过，Windows 对应测试目标严格 clippy、fmt、xtask、diff 检查通过；Linux 快照 `49ea8a35d471710da720cbfb419acfad59e656901bf2c3bbefd2f8d4d8732a94`。本批只补验证，没有改变生产缓存行为。
- 该证据仅涵盖可缓存新 200 的替换与后续命中。no-store/private、超限或读取失败的新响应与既有 fresh 条目的交互仍需单独设计/测试，不将可缓存分支通过推广为所有回源结果都已闭环。

第一百五十四批（2026-09-08，WEB 不可缓存替换与副本退休）：

- 检查发现 cacheable-request 回源得到新 200 后，Age/策略/正文收集失败分支直接返回，原 fresh 副本仍保留。新增副本观察 token 与退休 guard：仅持有 key/signature/独立 Arc 身份，不复制旧正文；新 200 返回后，处理结束或 guard 释放时只删除观察到的同一副本。
- 每次 put 都更换副本身份，正常替换或并发写入的新副本不被旧 guard 删除；访问更新 LRU 不改变身份。实际删除更新 bytes 和 Vary 索引，并推进现有全局 revision，阻止旧 in-flight fill 重新写回。这沿用保守全局失效，不引入无界按 URI tombstone；可能同时取消其他 URI 的旧填充。
- 新 HTTP 测试五种场景：private、no-store、无 freshness、非法 Age、正文超限。先确认 seed fresh hit，再强制回源取新正文，后续 GET 必须重新到源站取得 third 而非命中 seed。存储测试覆盖记账归零、旧 fill 拒绝、新副本保留、LRU 更新不改变身份、只退休匹配 Vary 副本。
- Windows/Linux cache/cache_race/cache_revalidate/cache_replacement 各 28 项通过，store 单元测试各 18 项通过；Windows 最终扩展的退休测试及严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `dbe798f5537b19bcdf28a88e7bbd571379fd7baee1b4703dad9633c32fe33c53`。
- 规则只作用于进入缓存主路径的新 200；认证/no-store 请求旁路、非 200、304 元数据合并不是本批扩展内容。正文错误及 await 取消会走 guard 清理，但尚无专门真实 HTTP 故障注入证明这些分支，不将超限用例等同于全部取消验收。

第一百五十五批（2026-09-08，WEB 缓存替换正文失败）：

- 新增真实 HTTP 故障源站，分别声明 Content-Length 12 但只发 7 字节、声明 chunk 大小 12 但未完成记录后关闭连接。先确认旧 seed 是 fresh hit，再以 no-cache 强制取得损坏的新响应。
- 对损坏响应使用独立 TCP 客户端并要求 3 秒内 EOF/读取错误，不将通用测试 helper 的读超时当成成功；随后普通 GET 必须回源取完整 third，源站关闭后再确认只命中 third，而非 seed 或 partial。
- Windows/Linux cache_replacement 各 2 项测试通过（新测试含两种 framing 故障），Windows 严格测试目标 clippy、fmt、xtask、diff 检查通过。Linux 快照 `5d897eaec2d255a6b98919d5e9238dfdba77cf56721fb3d7389feed30d62db84`。
- 本批补的是源站截断，不是客户端取消或响应仍在挂起时的 task abort；没有改变生产实现，后两者仍保留专项验收项。

第一百五十六批（2026-09-08，WEB 缓存收集任务取消）：

- 新增 cache/cancellation.rs，直接运行真实 CacheHandler/FlowCtrl 链，终端返回可缓存 200 和永久 Pending 正文。只有正文首次被真实收集器 poll 后才通过 oneshot 通知测试取消，避免用 sleep 猜测退休 guard 是否已经建立。
- 两种受控交错：无并发替换时 abort/join 后旧副本及 Vary 索引消失；收集中另写新副本时 abort 不删除新内容。正文资源有独立 Drop 通知，断言取消确实释放它，而非只检查 JoinHandle 状态。
- Windows/Linux 各 1 项测试（每项含两种交错）通过；Windows bundle all-targets 严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `a2bc6b00e5334b5ccd0e11ad91e3a59e041f276575b65fcc74ae07ebe73f3f2c`。
- 这是缓存 handler 的确定性 task-abort 验收，不是网络客户端取消验收；HTTP/2 RST_STREAM、H1 断连向该任务的取消传播和真实上游 socket 关闭仍需单独证明。不将模拟正文的 Drop 通知报告为真实回源连接已关闭。

第一百五十七批（2026-09-08，WEB / NET 缓存冷填充 H2 取消传播）：

- 新增真实 H2 客户端 → Gatel cache/proxy → 独立 H1 TCP 源站测试；源站发送可缓存 200、声明 1000 字节但只发送 partial，并保持连接不结束。客户端在尚未收到响应头时丢弃请求 future，要求源站连接 3 秒内 EOF/错误，未注入源站超时或主动断开来替代取消传播。
- 同一 H2 连接随后成功取得完整 new；源站 listener 退出后再确认缓存命中 new，避免半截正文入缓存或取消关闭了整个客户端连接。
- Windows/Linux cache_h2 各 1 项通过，Windows 对应严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `674494978f6fa5177e88ab6d83fee038696e7c9287f4b54d6c85212ddd0add96`。
- 本批是冷缓存 pre-head 取消，源站发送头并不单独证明缓存收集器当时已经 poll 正文；第 156 批另有确定性 handler abort 证据。两者不能合并宣称已有 warm 副本在真实网络取消瞬间的退休顺序已经证明，H1 断线也仍需独立测试。

第一百五十八批（2026-09-08，WEB / NET H1 abortive-close 取消）：

- 新增真实 H1 客户端 cache/proxy 冷填充取消测试，源站声明 1000 字节只发送 partial 后保持连接。客户端明确设置 SO_LINGER=0 并 drop，要求源站连接 3 秒内结束；不使用上传 shutdown/FIN 冒充不可继续接收响应的断开。
- 新客户端连接随后取得完整 new，源站退出后再命中完整缓存，证明断开没有缓存 partial 或破坏后续服务。该场景不验证已有 warm 副本的退休时序，也不宣称支持 FIN 半关闭语义。
- 仅新增 gatel-http dev-dependency socket2 0.6，离线解析使用锁内已有 0.6.5，锁文件记录直接测试依赖；未升级/下载新版本，生产依赖声明不变。
- Windows/Linux cache_h1 与 cache_h2 各 2 项通过；Windows 对应严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `2e3a17050d68f45e2b0c79dc6f191f7ebc53847ad58475e8809adcb7bf90ade4`。未执行其他平台或完整 workspace。

第一百五十九批（2026-09-08，QA-04 当前缓存实现短时流量/重载复验）：

- 在第 158 批 Linux 镜像 `2e3a17050d68f45e2b0c79dc6f191f7ebc53847ad58475e8809adcb7bf90ade4`，使用 `GATEL_SOAK_SECONDS=60 cargo test -p gatel-http --test soak --locked -- --ignored --nocapture` 实际运行现有定时测试，退出 0，耗时 60.03 秒。
- 完成 9382 轮（每轮 5 个业务请求，共 46910 个），每轮切换配置并协调 Supervisor/状态/指标；80 轮后基线为 state=4、metrics=187、sinks=1、idle=1、tasks=6、cached_resources=6，最终为 4/187/1/0/4/6，周期计数及最终排空断言均通过。
- RSS 记录从 18032 KiB 到 24732 KiB，仅观测没有内存硬断言。每轮顺序请求、debug profile、单进程同时含网关/后端/客户端，不能据此声明生产吞吐、独立网关 RSS 或零泄漏。
- 本负载的 API 使用唯一 URL，覆盖新副本分配和 generation 切换，但没有专门持续制造 warm 副本不可缓存替换、RST 或并发填充；第 154～158 批的专项正确性证据不能冒充这些场景的长稳证据。未执行小时级运行或 Windows/macOS soak。

第一百六十批（2026-09-08，WEB 元数据边界与职责拆分）：

- 将静态文件 ETag 与 HTTP 日期生成提取至 file_metadata.rs，文件 handler 不再内嵌日历换算。ETag 原有长度/mtime 格式保持不变，不新增内容摘要或强一致承诺；file_server 不再触发 xtask 的 600 行生产代码软提示。
- 原日期实现对 year 10000 仍返回字符串，新增拒绝边界断言先实际失败。现在复用已依赖的 httpdate 1.0.3，在调用前检查 epoch 下界和 253402300800 秒上界，范围外省略 Last-Modified；避免五位年份输出及该库的范围 panic，不增加依赖。
- 新测试覆盖 epoch、2000 闰日、2100 非闰世纪、9999 年末、亚秒截断和范围外。首轮 epoch 前 1ns 夹具在 Windows 被 SystemTime 舍入为 epoch，改为可表示的前 1 秒并断言确实早于 epoch，生产逻辑未因此放宽。
- Windows 文件相关单元测试 43 项、file_conditions/file_variants HTTP 测试 8 项通过；Linux 文件相关单元测试 45 项通过。Windows bundle all-targets 严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `cd3c9e4f7e18035ff7b2c55e9a55f73917f07efca6350c7b93d2423347229f2a`；未在真实文件系统创建 year-10000 文件或重跑完整 workspace。

第一百六十一批（2026-09-08，WEB 扩展名类型识别一致性）：

- 真实 HTTP 测试先复现 page.HTML 被识别为 application/octet-stream，而不是 text/html。静态文件 MIME 表查询现在仅对扩展名做 ASCII lowercase，路径本身不改写，不改变平台文件查找大小写规则；未知扩展名继续使用 application/octet-stream。
- 新用例覆盖 HTML/CsS/Js/JsOn/TXT/SVG 六种类型的 GET Content-Type，以及 HEAD 在启用 encode gzip 后保留对应类型、选择 gzip 且无正文；另验证未知 XYZ 不被猜测为文本。不把 HEAD 元数据验证等同于该新增夹具的 GET 压缩解码互操作。
- Windows/Linux file_variants 各 8 项通过，Windows bundle/http all-targets 严格 clippy、fmt、xtask、diff 检查通过。Linux 快照 `bad7770695c48005db601141bd97e341bef3c2b4d4aa2f9213e7291ce56af48e`；没有新增 MIME 依赖或扩展支持清单。

第一百六十二批（2026-09-08，混合大小写扩展名的 gzip 正文验证）：

- 扩展真实 HTTP 回归，覆盖 HTML、CsS、Js、JsOn、TXT、SVG 六类文件的 gzip GET；独立 Hyper 客户端解析传输帧，随后解压完整正文并逐字节比对源 fixture，不再仅以 HEAD 响应头证明动态压缩可用。
- GET 与 HEAD 的 Content-Type、Content-Encoding、ETag、Vary 一致；请求带 Connection: close，等待连接驱动正常退出，单次交换设五秒上限。此次未修改生产压缩实现。
- Windows 与 Linux 的 file_variants 均 8 项通过；Windows gatel-http 全 targets 严格 Clippy 通过。Linux 快照为 `85f354ff021df3375fd5b172bedc41a5778143643b6ec858b5f804a18bf4aa7e`。此证据不扩展为 Brotli/Zstd 正文或全工作区验收。

第一百六十三批（2026-09-08，OPS-01 配置格式化前端）：

- 新增 `gatel fmt [CONFIG] [--check]`，默认 `gatel.kdl`；stdout 输出或 CI 只读检查，不展开 import、不启动运行时、不依赖插件注册、不覆盖文件。保留注释与字符串值，支持幂等格式化；不是 v1 adapt 或递归/原地写入器。
- KDL 前端公开 `format_file`，CLI 不直接依赖 KDL 类型；lower/format 共用带嵌套限制的解析入口。配置读取器改用已打开文件的 metadata 与限长读取，补上仅按读取前大小检查的增长缺口；普通非文件路径在 open 前拒绝，不宣称恶意路径替换沙箱。
- Windows/Linux 各通过 KDL 24 项及实际 CLI 6 项，含格式/错误/不修改源文件/缺失 import/未知插件及自动生成补全、手册。Windows all-features/all-targets 严格 Clippy、xtask 通过；generation.rs 618 行软提示仍在。Linux 快照 `089e074a65ed4d504ba79495271689906a7e944b375701297cca7191e21f7edb`，未重跑完整 workspace 或 MSRV。

第一百六十四批（2026-09-08，KDL 格式语义与注释解析防护）：

- 增加格式化语义矩阵：禁用节点/属性/子块、嵌套注释、原始/多行字符串、类型标注、i128 边界、续行及非有限浮点数；去格式 AST 比较与幂等检查之外，独立核对 float bits，避免 KDL 自身相等判断将 NaN/无穷/有符号零归一化而掩盖变化。
- 检查锁定依赖源码发现块注释既有嵌套递归，也有连续片段递归；原预扫描只限制花括号。先运行 scanner 回归证实 17 层注释漏检（未故意触发进程栈溢出），再限制注释 16 层及每块 64 个解析片段。长普通文本注释仍允许；共享 guard 同时保护加载与 fmt。
- 实际 CLI 回归用 1000 层注释和 10000 个星号验证 fmt/--check/validate 正常拒绝，退出 1 而非异常终止。不是完整 KDL parser 安全审计或所有递归路径的证明。
- 注释内容断言进一步实际复现默认 autoformat 丢失 `/-` 禁用属性及行尾注释；增加格式片段保留层，保留 entry 原始格式和含注释的节点间隙/终止符，继续规范化节点缩进。不是仅依赖 AST 相等，也不承诺格式化所有行内空白。
- Windows/Linux 各 KDL 27 项、CLI 7 项通过；Windows all-features/all-targets 严格 Clippy 通过。Linux 最终快照 `eda37dba35e3539fa587d4d846b75a1a0cf8035373ade1e6145539a994ee1d98`。首次 Linux 注释保留测试失败已在该快照修复并重跑。

第一百六十五批（2026-09-08，KDL 换行边界一致性）：

- scanner 回归先实际复现 `// comment\r` 后的过深配置未被检查：旧实现只用 LF 结束行注释，而锁定 KDL 解析器支持八种换行。增加共享换行识别（LF、CRLF、CR、VT、FF、NEL、LS、PS），用于注释扫描与 LineIndex；CRLF 只增加一行，合法配置节点位置按真实 KDL 换行计算。
- 单元测试核对八种换行后的深度拒绝和合法节点 line=2/column=1。真实二进制回归以 1000 层配置覆盖八种换行 × fmt/--check/validate，均返回诊断退出 1；未在修复前故意执行栈溢出输入。
- Windows/Linux 各 KDL 28 项、CLI format 5 项通过；Windows all-features/all-targets 严格 Clippy 通过。Linux 快照 `33d5a956ecdddfbda57d66c2a207b43f8409416d75edd89cf188f236ad627243`。此批不等同于全解析器安全审计或全 workspace 验收。

第一百六十六批（2026-09-08，跨平台整仓回归复验）：

- Windows 执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，退出 0；96 个结果段累计 1275 passed、0 failed、4 ignored。没有把本命令解释为 doctest、MSRV、性能或外部服务验收。
- 将当前 crates/plugins/xtask 与根 Cargo.toml/Cargo.lock 完整同步至 Linux 验证容器，快照 `c5ac9e7093609b93cd5fa77796327d2c1a0f3830ba5de5a3636e1e81e8447f7c`。首轮同命令失败：gatel binary 的 15 项中 4 项在启动监听时遇到地址占用（secret rotation、account candidate ownership、TLS file reload、serve immutable frontend），11 项通过；没有删除或隐藏失败测试。
- 确认首轮进程与容器已退出后，同快照单独执行 `cargo test -p gatel --bin gatel --all-features --locked --quiet`，15 项通过；随后重新执行完整 workspace 命令退出 0。复跑首段输出截断，因此不报告 Linux 全仓精确通过总数。间歇性地址占用的根因尚未确定，复跑成功不能证明可靠性问题已修复；后续需针对测试地址分配与监听所有权继续诊断。
- 四个显式 ignored 入口仍是两个真实 PHP-FPM 测试、定时 soak、PHP 密码互操作；本批未另行执行这些入口。工作区仍有大量既有未提交改动，未切分支、提交、推送或发布。

第一百六十七批（2026-09-08，已绑定监听器所有权转移）：

- 新增 `BindTable::adopt(std::net::TcpListener)` 嵌入接口，按实际地址注册，不关闭重绑；重复地址拒绝且保留原 listener。使用同一进程级 socket 表，后续 ensure/reload/retain 无独立生命周期。需 Tokio I/O runtime，不自动读取系统继承 FD，也不等同于 systemd/Windows socket activation。
- 生命周期与 serve 单元夹具改为持续持有监听器后直接转交，移除原先“分配临时端口 → 关闭 → 配置重绑”的空窗。新测试在接管前建立连接并发送数据，接管后实际读取，检查重复注册不替换、ensure 复用与 retain 后释放引用。
- Windows/Linux bind 各 5 项通过；Windows gatel binary all-features 15 项通过，Linux 同目标连续 10 轮共 150 次通过。Windows all-features/all-targets 严格 Clippy 通过。Linux 快照 `ff71ac98d758003c39fcb46c1fcf34b15735d57a3b84fab5b39fb4138ad9cb7e`。
- 已从结构上消除此两处夹具的端口释放空窗，但上批失败时未取得端口占用者证据，不能将其根因全部归结为此问题；其他测试中的临时端口选择仍需逐项检查。此次未重新运行完整 workspace。

第一百六十八批（2026-09-08，接管监听器的真实 HTTP 事务验收）：

- 新增 adopted_listener 集成测试：接管之前连接并发送 HTTP 请求，首次候选在 file-server prepare 阶段失败；确认没有活动 generation/accept loop，原 socket 保留。有效配置发布后，之前排队的请求实际收到正确 HTTP 正文。
- 同一 socket 上验证新增占用地址导致 reload 失败时旧响应仍可用，恢复后新连接得到新响应，listener Arc 身份始终不变；显式 shutdown 后监听表/活动地址为空，弱引用确认 listener 无宿主持有者。整个场景设十秒上限，不用重复绑定端口作为所有权证明。
- Windows/Linux 各 1 项真实 HTTP 生命周期测试通过；Windows gatel-http all-targets 严格 Clippy 通过。Linux 快照 `e09fd258c87c368171d05c1ab024136124f44cc0c99c297144bc4401987330d1`。这不覆盖跨进程 FD 接收、TLS 接管或已有长连接上的权限撤销。

第一百六十九批（2026-09-08，CTL-01 来源快照条件合并）：

- 增加 `Snapshot::observe_if` 与类型化 `SourceUpdateError`，按来源 expected revision 合并可信前端输入，内部复用 SourceSnapshot::observe 的版本分配。None 表示来源必须未出现；相同内容不推进版本；清空节点保留来源与版本，不让删除后创建绕过旧写入条件。其他来源保持原样，冲突/耗尽不修改快照。
- GenerationStore 组合测试验证两个副本的按来源 CAS 虽可分别成功，旧整体版本仍不能提交；最新 admin 空意图触发 NoOp 后也使旧版本失效，文件修改必须合并回最新来源集合再发布。保留现有整代 CAS、注册策略和 prepare/commit 路径。
- Windows/Linux model/plan all-targets 各 241 项通过；Windows 两包 all-targets 严格 Clippy 通过。Linux 快照 `da2bb73e539ac62905d8827c4eb45bf35ecf8bcc9c8e47360501687520673ca7`。此接口不提供身份授权、共享锁、持久化、Admin HTTP 或已运行的多来源 CLI；CTL-01/02/03 的整体交付仍未完成。

第一百七十批（2026-09-08，CTL-01 跨来源引用真实 HTTP 验收）：

- 测试宿主增加完整 Snapshot 提交入口，与原 KDL 文件入口共用 prepare/commit/server sync，不直接修改运行时状态。新增真实 TCP 后端场景：文件来源的 route 引用 Admin 来源的 `runtime/shared`，通过 observe_if 合并两来源并正常代理。
- Admin 清空上游后产生明确 UnresolvedReference；已观察意图的 admin revision 推进到 2，但已发布代仍为 1、Arc 不变且旧 HTTP 服务可用。文件来源随后移除引用，保留 admin 空快照后成功发布，两来源版本均为 2，新请求返回独立响应且已发布 spec 不再包含上游。
- 初始夹具用了不被 Admin namespace 允许的 `shared`，被 OwnershipConflict 正确拒绝；夹具改用既有 `runtime/` 命名空间，没有放宽生产所有权规则。此批仍无 Admin HTTP 端点或持久化，测试宿主低层来源注册模式不代表真实 Admin 授权已完成。
- Windows/Linux source_composition 与 reload 各 7 项通过，Windows gatel-http all-targets 严格 Clippy、格式/diff 检查通过。Linux 快照 `0159cdfeb3414b113abf97ae10f01d8bbc7c274ad4edfc521fa8e1e33cf36de7`。未重新执行整仓测试。

第一百七十一批（2026-09-08，CTL-01 所有权元数据发布）：

- 实际回归复现：把同一 listener 的 global 块从 file:main 移到另一合法文件来源，site 保持不动且网络配置相同，原实现返回 NoOp，已发布 spec 的 owner 未更新。修复为 listener/site/upstream 的来源身份参与 spec 指纹，指纹域升级 gatel.spec.v2。
- owner 只进入发布指纹，不加入独立 transport/state compatibility；另一个测试验证同名上游跨合法文件来源转移会改变 spec/plan hash，但 Endpoints/Pool/Selector 绑定全部 Reuse。注释、源版本号、导入路径等仍不是发布差异，命名空间准入未放宽；不是跨租户授权机制。
- Windows/Linux plan all-targets 各 197 项、source_composition/reload 各 7 项通过；Windows plan 严格 Clippy 通过。Linux 快照 `c1cb56f734340bac44b10e539c42b64af044f016110ebc60c56e7c6ff8486dc1`。未重跑完整 workspace 或证明全部状态类型的来源转移策略。

第一百七十二批（2026-09-08，KDL 禁用前缀递归预算）：

- 检查锁定 KDL 源码发现 node_entry → slashdashed_entries → node_entry 的递归路径；原 scanner 不计算连续 `/-`。回归先证实连续前缀未被预扫描拒绝，随后将其与当前节点深度合计限制为 16。空白（含 Unicode）、注释、续行均不能重置前缀计数，独立有值禁用项则不累计递归层数。
- 单元测试覆盖七种分隔形式、100 个独立禁用项及字符串/原始字符串/行注释中的标记文本；CLI 增加 1000 个禁用前缀，验证 fmt/--check/validate 通过正常诊断退出而非异常终止。未在修复前故意触发依赖解析器栈溢出。
- Windows/Linux KDL 各 30 项、CLI format 各 5 项通过，Windows 两包 all-features/all-targets 严格 Clippy 通过。Linux 快照 `45de406da85b7907ad13b471d614733a2c212f1dc6b70f47653d118243c9604d`。同步纠正 scanner 注释中“绝不漏检”的过强表述；不是完整递归路径安全证明。

第一百七十三批（2026-09-08，原始字符串预扫描复杂度）：

- 原 skip_string 在每个候选结束引号之后用 filter/count 检查完整 hashes 长度，即使第一字节已经不匹配仍全部扫描。改为先确认后缀长度足够，再用 all 首个不匹配即结束，不分配临时后缀字符串；保留不完整后缀/真正结束符的原有语义。
- 增加 2048 个定界 #、16384 个假结束引号夹具，核对真实结束位置及其后超深节点仍会被发现，另覆盖不足长度及夹杂非 # 的后缀。Windows debug 同夹具单次 scanner 计时：修改前 138.6178 ms，修改后 328.9 µs；仅是本机诊断观测，不是整解析器/网关基准或稳定性能门禁。
- Windows/Linux KDL 各 32 项通过；Windows CLI format 5 项、KDL all-targets 严格 Clippy 通过。Linux 快照 `cc5fbb30315aa16a476f35069869168d2b6a9dc9f363a62d7fe54e0c84da616b`。未修改依赖解析器本身或宣称它全部路径具有相同复杂度。

第一百七十四批（2026-09-08，CTL-02 条件写入标签基础）：

- 新增 Version::etag 与 GenerationStore::attempt_if_etag，不依赖 HTTP 框架。标签包含完整 BootId（不是日志缩写）、generation、排序来源版本向量，固定域与完整 256-bit 指纹；不暴露来源名，不是身份凭据或含 uptime 等动态响应的表示校验器。
- 标签比较后仍委托原 attempt_if，在观察锁内核对完整 Version，防止比较/提交空窗。核心接口只接受精确单个强标签；弱标签、列表、通配符、裸标签及额外空白均冲突，意图和资源不改变。HTTP 层需另行定义头解析/状态码与授权，不能把该接口当作完整 If-Match 协议实现。
- 测试覆盖不同进程生命周期、失败意图、NoOp、同意图准备失败后的成功重试、拒绝输入不推进 head、不调用 prepare，以及两个并发写入者共用标签只成功一个。Windows/Linux model/plan all-targets 各 246 项通过，Windows 严格 Clippy 通过。Linux 快照 `62a60ec20803b3bd0662c9b7d55556615125f2bc8d820548644de01e85caa844`；Admin listener、认证/授权、资源 HTTP API 和持久化仍待实现。

第一百七十五批（2026-09-08，配置存储实例身份加固）：

- BootId 生成加入 checked AtomicU64 进程内序号，去掉 Instant::now 后立即 elapsed 的不可靠区分因素；时间戳以完整 seconds/subsec_nanos 编码，不再把 u128 总纳秒截断到 u64。增加固定域，序号耗尽明确失败，不绕回旧身份。
- 新测试覆盖固定时间下不同序号、不同 PID、相差 2^64 纳秒的时间戳，以及连续生成 100000 个身份。修改前实际快速生成测试通过，未宣称复现碰撞；此项依据生成规则进行加固。跨重启完全相同 PID/时间、进程镜像克隆或密码学随机性仍不在保证范围。
- Windows/Linux model/plan all-targets 各 248 项通过，Windows 严格 Clippy 通过。Linux 快照 `f26745d4403e13a715a3a4cadba05f553c75bd29e33674e32c731905e0fb45c4`；未执行整仓、fork/快照恢复或真实管理 HTTP 验收。

第一百七十六批（2026-09-08，命名文件来源接入实际宿主）：

- `run`、`validate`、`explain` 支持重复的 `--source NAME=PATH`，主来源固定 `file:main`，最多 31 个附加来源；宿主只接受启动时注册的来源，重复名称不覆盖。
- 多文件在同一观察锁内构建完整 Snapshot，读取/语法失败不推进部分来源版本；各来源独立版本，联合编译与原有 prepare/CAS 发布链一致。业务资源路径仍相对主配置目录，import 相对各自文件。
- watcher 启动时订阅所有已发现文件的父目录，后续外部 import 由五秒轮询兜底。真实 HTTP 回归覆盖附加来源发布、损坏后保留旧响应、恢复后独立推进版本，以及实际 watcher 循环发布新响应；不把兜底成功宣称为通知时序保证。
- 真实 CLI 测试覆盖联合 validate/explain、缺失来源、重复/保留名称，以及 run --dry-run 的准备与关闭。固定 8080 夹具在 Windows 因占用失败，改为 OS 分配的非零端口；CLI 尚无 socket adoption，释放保留 socket 到子进程绑定之间仍存在小窗口。未放宽生产端口非零校验。
- Windows 与 Linux 全 feature 的 gatel binary / cli_artifacts / file_sources 合计各 22 项通过；Windows 严格 Clippy 通过。Linux 最终镜像 `1f2031c7ee0f8432478437ebc97286bcab1dfa270742825d5956735ff9c064e2`。此次不是完整 workspace 回归。
- 使用和限制见 CLI.md。多文件读取不等于文件系统原子快照，来源集合不能在线增删，fmt/diff 暂无多来源入口；Admin HTTP、认证与持久化仍未完成。

第一百七十七批（2026-09-08，多来源变更预览）：

- `diff BEFORE AFTER` 增加可重复 `--before-source NAME=PATH` 与 `--after-source NAME=PATH`，两侧分别走实际宿主的命名文件观察与联合编译，不再只能比较主文件。旧单文件用法不变。
- 两侧来源列表独立，不做隐式继承；来源重命名保留既有所有权语义。比较只编译，不执行 prepare、不绑定监听，也不等同于支持运行时动态注册来源。
- 实际 CLI 测试覆盖相同联合配置 no change、附加来源中的 endpoint 变化导致状态重建、变更后来源文件缺失退出 1；整个比较期间测试持有配置端口，确认没有绑定副作用。
- Windows/Linux 全 feature 的 gatel binary / cli_artifacts / file_sources 各 22 项通过；Windows 严格 Clippy 通过。Linux 镜像 `cf616bbfdba2e438df7f7e0310fea21e851c53fc0a4b6d1ecbd42faf9d4ad7fc`。使用和退出语义同步 CLI.md；不宣称全 workspace 或运行时资源可用性验收。

第一百七十八批（2026-09-08，变更预览区分资源变化与状态复用）：

- 回归先复现：上游从 `file:backends` 移交至 `file:renamed`，计划已变化但 diff 仅输出 state/all reused。修复为新增 resources 摘要，独立展示资源增删、所有权迁移与规范化配置变化；覆盖 listener/site/upstream 和 global，保留原有状态复用决策。
- 使用既有规范化资源哈希在内部比较，不打印插件配置值或其哈希；资源级摘要不宣称完整字段级解释。站点摘要包含路由及中间件变化，输出 state/all reused 不再成为唯一变化提示。
- 真实 CLI 回归覆盖所有权迁移且状态复用、响应正文改变且状态复用、输出不包含测试正文。相同配置仍为 no change。Windows gatel binary/CLI artifacts/多来源测试通过，追加响应回归单独复跑通过，严格 Clippy 通过；Linux 同组 22 项通过，镜像 `f974b9712bb49dfde4e2f69fdb9af30200b73c5a9c1d77862a004f3939f74436`。

第一百七十九批（2026-09-08，文件监听故障退化节流）：

- 回归先复现通知通道关闭后等待耗时为 0 而非五秒：原 select 把 recv 的 None 当作配置事件，监听创建失败后可能每个 200ms 消抖周期重新读取配置。修改为只接受 Some 事件，关闭分支失配后保留原轮询计时器；主循环继续独立响应关闭信号。
- 抽取实际使用的 wait_for_change，新增三项虚拟时钟回归：已关闭通道连续三轮均等待五秒；排队通知立即唤醒且无事件时轮询；等待两秒后通道关闭既不提前唤醒也不重置五秒期限。Tokio test-util 仅为开发依赖，不增加生产 feature。
- Windows/Linux 全 feature 的 binary / cli_artifacts / file_sources 各 25 项通过，Windows 严格 Clippy、xtask 检查通过；Linux 最终镜像 `d45b93ef024a51def5038668f6778c9ccf019bbe5c31ac32f65600ed880432f5`。这些测试直接覆盖关闭通道与真实 watcher 正常链路，不宣称模拟了所有操作系统监听器失败方式。

第一百八十批（2026-09-08，动态协调配置目录订阅）：

- watcher 从启动时一次性递归监听改为已加载配置文件父目录的非递归订阅，每次重载尝试后协调集合。新增先于移除，同目录去重；只有操作成功才更新已订阅集合，失败后续重试。避免连带递归监听静态内容或构建目录，五秒轮询继续承担通知遗漏和新目录发现兜底。
- 两项确定性订阅操作回归覆盖去重、不重复执行、添加/移除顺序及失败重试。真实 HTTP 回归增加启动后创建子目录并 import、随后修改导入文件，两次变化均发布，来源版本最终为 5。
- 初版测试误用绝对路径 import，被现有解析器按预期拒绝；改为合法子目录 import，未放宽相对路径/禁止 `..` 的规则。此前“外部 import”的文档表述已纠正：外部根通过 --source 注册。实际 HTTP 验证不区分通知和轮询的触发方式，不保证所有平台的目录删除重建通知可靠。
- Windows/Linux 全 feature binary / cli_artifacts / file_sources 各 27 项通过，Windows 严格 Clippy 通过；Linux 镜像 `2c87b893de83eef2b691b40e7ebba6ac2c159bad478c77c357fafcba2052f700`。新增/替换的事件同步 EVENTS.md；仍非完整 workspace 或完整控制面交付。

第一百八十一批（2026-09-08，近期配置与运行改动的完整工作区回归）：

- 对当前工作树运行 `cargo test --workspace --all-features --all-targets --locked --quiet`，不以近几批宿主局部测试代替完整组合检查。首次 Windows/Linux 并行执行均失败：Windows 完成 674 passed / 1 failed / 2 ignored 后在 SCGI 的 `configured_idle_timeout_applies_before_no_body_response_is_published` 停止；Linux 宿主 22 passed / 1 failed，在 `failed_account_candidate_does_not_replace_active_shutdown_ownership` 的 activate 一秒等待中超时。
- 确认原任务退出后，Windows SCGI 原测试单跑通过（1.02s）；Linux 原镜像 ACME 测试连续单跑十次均通过（0.01～0.07s）。未改生产实现、未放宽超时、未删除断言。首次并发执行与超时的相关性不是根因证明，仍需在受控负载下复现和定位。
- 随后按平台顺序完整重跑：Linux 1316 passed / 0 failed / 4 ignored；Windows 1301 passed / 0 failed / 4 ignored，均取得完整 99 段 test result 且进程退出 0。Linux 镜像仍为 `2c87b893de83eef2b691b40e7ebba6ac2c159bad478c77c357fafcba2052f700`，无中途代码修改。
- Windows `cargo clippy --workspace --all-features --all-targets --locked -- -D warnings` 通过。四项 ignored 分别为协议层 PHP-FPM、HTTP 层 PHP-FPM、PHP 密码互操作与定时 soak；本批未重新执行这些项目，也不包含 doctest、MSRV、macOS、外部 CA 或远端 CI。
- QA-04 新增待定位项：受控 CPU/I/O 压力下复现上述两处时限失败，记录准备/激活/请求阶段耗时，区分产品响应期限和测试防挂死期限；重跑通过不能关闭此项。CTL-02/03 等未完成的大项仍按原规划保留。

第一百八十二批（2026-09-08，控制面候选接入真实宿主事务链）：

- 提取并接通 `App::attempt_snapshot_if(expected, snapshot)`：可信宿主提供完整来源集合和整代版本，统一复用生产 Resources/prepare、插件退休、ACME 工厂、外部材料 refresh 与原子发布；不另造旁路运行时。文件 attempt 同样委托该入口，版本捕获前移至来源观察之前。
- 前置版本检查拒绝已过期候选，不执行其资源准备；资源观察后的 store 条件事务再次检查版本，避免 check-then-act 间隙。成功提交后的 activate 保持既有显式步骤，未虚称发布与外部持久化原子一致。
- 新增真实宿主 HTTP 回归：显式注册 file/admin 来源、初始提交、悬空引用候选失败仍服务旧响应、失败意图使旧写入版本失效、恢复发布包含 Admin 上游、过期重放不改变版本、同版本同意图 NoOp、正常 shutdown。
- Windows 宿主 24 项、Linux 宿主与 CLI artifacts/file_sources 共 28 项通过；Windows 宿主严格 Clippy 通过。Linux 镜像 `12dd9056284abec3a4f6eeb4ddcb84128e29d80af36571453791a8447dc7be27`。此批不是完整 workspace 回归。
- 这是内部可信调用接口，不开放网络，也不隐式修改来源缓存；协调器仍需持有完整意图并落实授权、审计、限额和持久化，Admin HTTP/恢复协议仍未完成。修正 model 模块“binary 只有单一来源”的过时注释，集成边界同步 ARCHITECTURE.md。

第一百八十三批（2026-09-08，关闭期间禁止配置重新发布）：

- GenerationStore 增加幂等、不可逆 seal，使用与提交相同的写锁；新观察、NoOp、refresh 和已进入 prepare 的迟到成功/失败均受封闭检查约束。当前 generation 与已观察意图保留给读者，不用清空运行时模拟关闭。
- App.shutdown 在等待生命周期 mutex/排空之前 seal；宿主条件提交前置检查封闭状态。封闭返回 Stale，不应重试同一 store；它是生命周期事实，不伪造新配置 revision 或 ETag。
- 两项 plan 回归覆盖所有四种提交入口封闭后不观察/不 prepare、幂等封闭、读者保持、prepare 中封闭后的成功/失败均不能发布或覆盖失败记录。真实宿主回归扩展关闭后的条件提交与 activate，要求版本/Arc 不变且监听仍为空。
- Windows/Linux plan all-targets 202 项与宿主 24 项通过，Windows 两包严格 Clippy 通过。Linux 镜像 `2eb7adf97464e384c68ab47b0e3cd280ed31d96bd2d5f34dafe9b4ffbaf50a1d`（最终补充的 Outcome 注释不影响测试代码）。本批不是完整 workspace 回归。
- seal 只封闭观察/发布，不中断或等待同步 prepare；已进入 prepare 的候选仍可能完成资源获取再清理。后续控制面请求协调器必须跟踪并排空在途提交，不能将该栅栏单独宣称为“所有配置任务已停止”。此边界已写入 ARCHITECTURE.md。

第一百八十四批（2026-09-08，宿主在途配置提交排空）：

- App 条件提交增加 RAII admission，覆盖外部资源观察、prepare、发布与同步清理；新提交检查与在途计数在同一 mutex 中完成。shutdown 先关闭 admission、seal store，再与 activate 串行、标记 draining，等待在途计数归零后排空 server/supervisor/ACME/plugin tasks。
- 取消等待不会丢失计数或重新开放入口，后续 shutdown 可重试。Notify 在读取计数之前注册，最后一个 admission 释放后唤醒所有等待者；正常返回与 panic unwind 都会释放 admission。
- 四项新增测试覆盖取消/并发等待、最后释放真实唤醒、panic unwind 清理，以及实际 App 的关闭等待被取消后重试、期间拒绝候选且不重新开放提交。既有真实 HTTP 条件提交和 watcher 测试继续通过。
- Windows/Linux 宿主与 CLI artifacts/file_sources 各 32 项通过，Windows 宿主严格 Clippy 通过；Linux 镜像 `e0adbc56bc05b67ee5cbc81792bd6024d0eacc4e936b269ffd35c811e9b37aa3`。不代表完整 workspace 或网络 Admin 验收。
- 此排空覆盖 App 提交入口，不覆盖绕过宿主直接写低层 store、未跟踪插件工作或提交前文件解析；不强杀同步线程。永久阻塞的 prepare 可能让 shutdown 永久等待，外部终止策略仍是独立职责。ARCHITECTURE.md 已更新，替换上一批“宿主尚不等待在途 prepare”的现状描述。

第一百八十五批（2026-09-08，异步宿主的有界配置工作线程）：

- 启动 run/serve 和 watcher 主重载改用 App.attempt_async，将来源读取、编译、资源观察和 prepare 放到 Tokio blocking 任务；每个宿主一个 semaphore 名额，等待队列不占用 blocking 线程。同步入口仍保留给可信嵌入与测试。
- admission 改为拥有 Arc 的 RAII guard，与执行名额一起移入 blocking closure；等待者被取消/JoinHandle 丢弃不会提前归还名额或让 shutdown 漏掉后台工作。关闭 semaphore 唤醒尚未取得名额的调用并拒绝读取配置，在途 worker 仍由提交排空跟踪。
- 新增单线程 Tokio 真实阻塞任务回归：worker 等待同步通道时异步侧可继续执行；取消调用者后执行名额仍占用，第二次提交等待；shutdown 拒绝排队提交且未读配置文件，直到释放 worker 后才完成。
- Windows/Linux 宿主与 CLI artifacts/file_sources 各 33 项通过，Windows 宿主严格 Clippy、xtask 检查通过；Linux 镜像 `ff063c869bea0807128d17ad9e5c37bbc06419bb1da15cf9a2d2886f0ee64d6a`。此批非完整 workspace 回归。
- 目录订阅发现仍同步，尚非 watcher 全路径非阻塞；同步调用者不受异步 semaphore 限额但仍参与 drain。worker panic 继续传播，不错误地宣称 prepare 失败且必然未发布。客户端取消后继续负责 activate 的网络控制面协调仍待实现，相关边界同步 ARCHITECTURE.md。

第一百八十六批（2026-09-08，配置目录发现复用受控后台执行）：

- 将有界 blocking 入口抽为泛型 configuration_work；重载与 watcher 文件发现共享同一 semaphore/admission/关闭排空，不单独创建无约束扫描任务。闭合入口返回 None，主重载转为 Stale，目录发现直接跳过协调，不误用空目录集合删除现有订阅。
- 将已加载文件的父目录提取、canonicalize 与配置读取一起放入后台任务，目录集合差分及 OS watch/unwatch 仍由 watcher 协调执行；不宣称所有 OS 监听调用非阻塞。
- 新增回归验证扫描不在单线程 Tokio reactor 线程执行、读取失败保留根文件监听路径、扫描不推进来源观察缓存、关闭后扫描闭包不运行。沿用取消等待者仍占用执行名额/排空计数和真实动态 import 回归。
- Windows/Linux 宿主与 CLI artifacts/file_sources 各 34 项通过，Windows 严格 Clippy（修正 question_mark lint 后）与 xtask 通过；Linux 镜像 `31a962d88709b43a48501fa62475e420f4d3f487b0215a868dee9cb168d7aaba`。未运行完整 workspace；CLI.md 与 ARCHITECTURE.md 同步当前执行边界。

第一百八十七批（2026-09-08，重载全周期持续等待关闭信号）：

- watcher 用同一个关闭 future 包围初始发现、事件等待、消抖、重载、激活和订阅刷新，不再只在循环间隙创建信号等待。两个分支同时就绪时优先关闭；监听流程退出后由现有 run_app 调用宿主 shutdown，封闭并排空已进入的 blocking 任务。
- 新增注入式关闭回归：预占配置执行名额，让初始扫描无法执行；发出关闭通知后 watcher 在名额仍被持有时退出，来源缓存未读取。沿用上一批后台 worker 取消与 drain 测试，不将此注入式测试当作真实 OS 信号测试。
- Windows/Linux 宿主与 CLI artifacts/file_sources 各 35 项通过，Windows 严格 Clippy 和 xtask 检查通过；Linux 镜像 `84dd0be7660fda89de3757ac267fbbd70a0fefc1e1c59a3eadfec9213ea5b723`。不包括完整 workspace、Windows 控制台 Ctrl+C 或本批独立 SIGTERM 进程验收。
- 同步 OS watch/unwatch 不能在一次 poll 内被抢占；本次也不覆盖进入 watcher 之前的 CLI 初次配置启动。ARCHITECTURE.md 已明确这些边界，未改变永久阻塞 prepare 需要外部终止策略的限制。

第一百八十八批（2026-09-08，真实 run 进程热重载与 SIGTERM 验收）：

- 新增 run_reload 集成测试，通过真实 binary 的 run --source 启动监听。修改附加来源为语法错误，等待实际 config_rejected 事件再断言旧 HTTP 正文仍可服务；恢复来源后等待新正文，避免只测试尚未处理坏配置的短暂窗口。
- Linux 向自己的子进程发送真实 SIGTERM，要求退出码 0、记录 shutdown_complete、原端口不再接受连接；测试有超时和 RAII 子进程/专用临时目录清理。Windows 验证同样的重载链路，但结束使用 kill，不宣称 Ctrl+C 优雅关闭。
- Windows 独立测试通过，宿主 all-targets/all-features 严格 Clippy 通过；Linux 首次通过后同镜像再连续五轮均通过。镜像 `a0bad0a1a58bd86f68217f39a6bc20df7b84f5deb9fb4a15697dafbe8b448a66`。本批不是完整 workspace 回归，也未修正第一百八十一批间歇性超时的根因。
- 测试在恢复发布后发送 SIGTERM，不宣称覆盖任意 prepare 中断时序。CLI 无 socket adoption，OS 分配端口释放到子进程绑定之间仍有小窗口；没有通过放宽非零端口配置规则规避此限制。CLI.md 已同步验收边界。

第一百八十九批（2026-09-08，关闭后的监听 socket 所有权释放）：

- 在真实宿主条件提交回归中追加关闭后的 BindTable/弱引用断言，先复现 bound 非空：accept 任务已退出，但存活 App 的绑定表仍持有 socket，不能把“无活动 listener task”视为端口已经释放。
- App.shutdown 在在途提交与 server accept/retirement 排空后清空 BindTable，保留当前 generation 的可检查性；同步准备已经被关闭入口挡住或排空，不让宿主在清空后由迟到 prepare 重新加回绑定。
- 新增接管 socket 后未激活就关闭的回归，要求重复 shutdown 幂等、绑定表为空、listener 弱引用失效且同地址可重新绑定；已有真实 HTTP 回归在仍持有 App/committed 时确认 listener 所有权释放。
- Windows/Linux 宿主 32 项和 run_reload 实际进程测试 1 项均通过，Windows 严格 Clippy、xtask 检查通过；Linux 镜像 `ec9d884a71f92d08eb31b52921bf33aca4dd37863e1b2e9742ab5d3df6bb8680`。此次非完整 workspace 回归。
- 外部持有 listener Arc/复制 OS descriptor，或绕过 App admission 直接调用 BindTable 的代码不在此保证内；宿主不能撤销外部句柄。该所有权边界同步 ARCHITECTURE.md。

第一百九十批（2026-09-08，重载及激活由宿主任务持有）：

- 启动与 watcher 改用 App.reload，内部 async task 持有外层 admission，覆盖主事务及成功后的 activate。调用者取消只丢弃等待，不取消已接受的重载/激活任务；底层阻塞工作仍保持原有名额与排空跟踪。
- shutdown 先在生命周期锁内标记 closed/draining，再释放锁等待 admission 归零，最后重新取锁执行资源排空。否则新纳入跟踪的激活可能等待生命周期锁，而 shutdown 持该锁等待激活释放 admission，形成循环等待。
- 新增两项回归：持有激活锁、等待实际配置提交后取消调用者，释放锁后真实 HTTP 仍返回新正文；模拟排空时的排队激活，要求其能取得锁看到 closed 并释放 admission，shutdown 不死锁。
- Windows/Linux 宿主 34 项与 run_reload 真实进程测试 1 项通过，Windows 严格 Clippy 通过；Linux 镜像 `f2fb1d5d008d39c9f4ace34632383abe481800df5654f04f1c7cedfa28556c9a`。本批非完整 workspace 回归。
- 这是进程内任务所有权，不等于持久任务、网络请求队列限额或已实现 Admin API；低层 attempt_async/attempt_snapshot_if 仍要求调用者显式负责 activate。ARCHITECTURE.md 已同步两阶段关闭与新入口边界。

第一百九十一批（2026-09-08，重载任务总量准入上限）：

- 为 App.reload 增加独立的总量 semaphore，上限 8，覆盖等待执行、prepare 和待激活任务；在 spawn 之前 try_acquire，超额直接返回带容量原因的 Stale，不读取配置、不推进来源版本。不将“一条 blocking 工作线程”误当作“排队任务已有限额”。
- 额度随宿主持有任务结束才释放，调用者取消不会提前释放；shutdown 关闭总量入口，与已有执行名额、admission 排空协作。现有同步/低层可信接口不伪称受此限额控制。
- 新增确定性回归：预占执行名额，接收 8 个重载后丢弃所有等待者，第 9 个仍因容量拒绝且版本/文件观察缓存不变；释放执行名额后任务结束并恢复全部额度，关闭后仍拒绝新重载。
- Windows/Linux 宿主 35 项和真实 run_reload 进程测试 1 项通过，Windows 严格 Clippy 通过；Linux 镜像 `56fb28275b72e91fcd19078b956ebbbb3fcd67dc7b6c6c71edfc271d1d1c3c6a`。此批非完整 workspace 回归。
- 固定的宿主任务上限不是网络请求队列/正文大小上限，未来 Admin adapter 仍需请求准入与明确过载响应。边界已同步 ARCHITECTURE.md，未增加当前不存在的 HTTP 端点或伪配置选项。

第一百九十二批（2026-09-08，事务核心按职责拆分）：

- 将 attempt_conditionally 原样迁入内部 generation_observe.rs，集中来源准入、封闭/版本前提、来源数量、同版本内容身份和完整 head 合并规则；仅将方法可见性改为父模块内部可用。GenerationStore 的公共 API、锁、CandidateBase 与后续 prepare/publication 路径保持不变。
- 此拆分让后续文件/Admin 等前端共用规则有明确维护位置，不引入新万能接口、旁路存储或额外锁；generation.rs 不再触发 600 行目标警告，xtask check 输出 ok 且无原有文件尺寸提示。
- Windows/Linux plan all-targets 202 项、宿主 35 项及真实 run_reload 进程测试 1 项通过，共各 238 项；Windows 两包严格 Clippy 通过。Linux 镜像 `e825920eda69b4fd0784bc7da1d65298c9971d0e68386460d85187d1f645581b`。不是完整 workspace 回归，不把结构整理计为新增 Admin 功能。
- 模块职责同步 ARCHITECTURE.md；控制面认证、持久化及其 HTTP adapter 等既有未完成任务保持原范围。

第一百九十三批（2026-09-08，过滤无关文件的修改事件）：

- watcher 根据后台发现得到的配置文件名集合过滤普通 Modify 通知，通知线程不做文件 I/O；避免无关日志或静态文件普通写入直接触发配置重读。同名文件保守接受，不按扩展名猜测配置，也不宣称精确路径匹配。
- 保留创建、删除、重命名、Any、rescan 与无路径 Modify；发现尚未完成时保守接受。发现失败导致未列出的 import、外部 Secret/证书及平台路径拼写差异由原五秒轮询兜底，未取消这些刷新路径。
- 新增纯事件矩阵回归覆盖日志写入忽略、任意扩展名配置接受、同名保守接受、拓扑变化保留和 Access 忽略。真实 run_reload 增加坏配置与日志同目录下的短窗口检查，同时继续验证旧响应、恢复发布与 Linux SIGTERM。
- Windows 将所有 Modify 放行的临时对照运行也通过短窗口测试：本机未稳定复现日志自激循环，因此不宣称“先复现并修复该循环”。对照已撤销并重新编译验证；过滤普通无关修改的规则由确定性单测直接证明。
- Windows/Linux 宿主 36 项和真实 run_reload 1 项通过，Windows 严格 Clippy 通过；Linux 镜像 `8b564541831026207f10204a34b1733051b7c1e23bfc9a3edbb9a87580259c72`。本批非完整 workspace 回归，过滤边界同步 CLI.md。

第一百九十四批（2026-09-08，完整来源条件提交的有界异步入口）：

- 新增宿主内部 reload_snapshot_if，接收完整授权 Snapshot 和整体 Version，共用文件 reload 的总量准入、后台配置执行名额、任务所有权与激活/关闭排空。公共重载逻辑提取为内部 reload_task，避免两条前端各自实现生命周期。
- 版本检查在后台执行时进行，排队期间 head 变化返回类型化 Conflict；容量或关闭拒绝仍为 Ok(Stale)。不可变 CLI 输入经新入口提交，文件输入仍在观察前捕获版本。调用者取消不放弃已接受任务的激活。
- 新增排队后旧版本失效回归，并在原容量测试中验证两种入口共享额度；完整来源真实 HTTP 回归改用异步入口，移除调用者显式 activate，覆盖首发、拒绝后旧响应、恢复、新旧版本冲突及 NoOp。既有取消后真实 HTTP 激活测试通过不可变输入覆盖新路径。
- Windows/Linux 宿主 37 项和真实 run_reload 1 项均通过；Windows 严格 Clippy、xtask check 和 diff whitespace 检查通过。Linux 镜像 `0a514002ffc5903993b9e8d4821286f57e9c45142951c0c5ec410273784d0ef2`。此次不是完整 workspace 回归。
- 入口不持久化 intent、不替换文件或不可变来源缓存，不是公开 embedding crate、Admin HTTP 认证/请求限额或持久化恢复；这些控制面任务保持未完成。ARCHITECTURE.md 已同步边界。

第一百九十五批（2026-09-08，可读取的最新意图与单来源条件组合）：

- GenerationStore 将原指纹表改为保留最新已准入 SourceSnapshot 的 Arc 表，和 head 使用同一把事务锁更新；observed_snapshot 原子取得 Version/完整来源联合，配置树复制与替换后释放不在事务锁内进行。最新意图与活动 LKG 明确分离。
- 新增 plan 层 attempt_source_if，更新一个完整来源贡献时保留其他来源，包括被拒绝的意图；组合后仍走完整 Version 条件事务，不自动重试冲突、不悄悄回退其他来源到旧成功配置。空来源保留 tombstone 版本。
- 三项新增回归覆盖另一来源失败意图保留及修正恢复、过期版本与旧 tombstone 拒绝、不完整过期联合的现有 head 合并语义、封闭后不改意图、同版本指纹冲突不部分替换来源树，以及读取快照独立性。
- Windows/Linux plan all-targets 各 205 项、宿主 37 项和真实 run_reload 1 项通过，共各 243 项；Windows 两包严格 Clippy、xtask check 与 diff whitespace 检查通过。Linux 镜像 `cbe13415849661ae83b72c7bc0294bf5c13d9115ed54d62574b9bfc817d9d3f0`。本次不是全 workspace 回归或性能验收。
- 保留树增加内存和复制成本，不将来源数限制等同于正文大小限制；读取树可能暴露配置秘密，必须由前端授权，不能用作公开 status。此低层 API 不代替宿主资源刷新/激活，不负责前端缓存同步、审计或磁盘恢复。ARCHITECTURE.md 已同步这些边界。

第一百九十六批（2026-09-08，意图并发验收与重复复制消除）：

- 新增跨线程、通道控制的准备时序回归：文件来源的准备暂停期间，另一个来源提交非法新意图；旧准备随后成功或失败均返回 Stale，活动 LKG、最新两来源意图及新失败的 stage/summary/时间戳均不被覆盖。读出的旧快照仍保持原值，测试等待有超时。
- 保留配置树前先短暂读取单调 head，仅复制尚未观察过的新来源版本；复制仍在锁外，写锁内仍完整执行准入和版本检查。并发推进最多使复制冗余，不会让已观察版本变成新版本；未变化/过期来源不再每次轮询重复复制。新增直接覆盖选择规则的测试，不据此宣称已测量吞吐或内存收益。
- Windows/Linux plan all-targets 各 207 项通过，Windows 宿主 37 项与真实 run_reload 1 项通过；Windows plan 严格 Clippy、xtask check 通过。Linux 镜像 `5caf4cb5b434b311642dab7430a088667375aa7432a66ad66d757b1d19c2ec97`。本批未重跑 Linux 宿主、全 workspace 或性能基准。
- ARCHITECTURE.md 更新选择性复制与并发重检边界；控制面的授权、持久化、恢复和前端缓存同步仍是后续任务。

第一百九十七批（2026-09-08，文件重载组合非文件来源意图）：

- 先用真实宿主 HTTP 回归复现：可信宿主注册并提交 Admin 来源后，修改主配置文件不能产生 Reloaded；文件前端原来提交的集合遗漏了已观察来源，事务拒绝不完整集合，并非静默覆盖运行时来源。
- App.attempt 现在在文件观察前原子捕获 Version/最新完整意图，只替换本次文件前端观察到的来源贡献，再经原宿主条件提交完成资源刷新/准备/发布。另一来源的非法意图与空 tombstone 都保留；不通过观察后重新取版本掩盖并发冲突。
- 新增回归覆盖注册运行时 upstream 后文件响应更新且资源保留；运行时来源被拒绝后再改文件，旧 HTTP 继续服务；修正运行时来源为空后发布此前文件意图，随后文件重载 NoOp 且 tombstone 版本仍在。测试使用接管的 OS 分配端口与专用临时目录。
- Windows/Linux 宿主各 38 项与真实 run_reload 1 项通过；Windows 严格 Clippy、xtask check 和 diff whitespace 检查通过。Linux 镜像 `b997d8d6e58da29d3101aafdffbfbec9465529dd5821d1d7accd5ff898d4628b`。本次非完整 workspace 回归。
- CLI 未自动注册 Admin 或开放网络管理端点；运行时写者不能冒充文件来源。持久化、授权和前端写入身份约束仍需后续完善。ARCHITECTURE.md 同步文件观察前组合与来源所有权边界。

第一百九十八批（2026-09-08，多来源改动后的全工作区回归）：

- Windows 与 Linux 顺序执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，均等待原进程终态，未因无输出重启。Windows 1324 passed / 0 failed / 4 ignored，Linux 1339 passed / 0 failed / 4 ignored；各 100 个测试结果组，退出码均为 0。
- Linux 验证前重新同步工作区 crates/plugins、Cargo 清单与锁、xtask、examples、docs 等，镜像 `a59c29ed04b8ec46e54161ff73b467edcd17b19fc8289eac939516a61909e154`。Windows rustc 1.98.0；不将本次本地验证记为远端 CI、macOS 或 MSRV 验收。
- 四项忽略保持原样：协议 PHP-FPM、HTTP PHP-FPM、PHP 密码互操作、显式 soak，未计为通过。Windows 全工作区 all-features/all-targets 严格 Clippy 和 xtask check 通过。
- Linux 全量中的 precompress 六项测试组耗时 62.89 秒，两项出现超过 60 秒提示但最终通过；同镜像独立重跑该组六项全部通过、耗时 0.32 秒。此差异未定位根因，未放宽测试或据重跑宣布修复，需后续在受控 CPU/I/O 环境记录子进程与压缩阶段耗时。
- 第一百八十一批记录的 SCGI/ACME 间歇性超时本次没有再失败，但根因仍未确认；QA-04 不据此关闭。整体功能恢复、控制面、发行与外部验收任务仍保持原范围。

第一百九十九批（2026-09-08，预压缩阶段诊断与测试子进程等待）：

- 新增 debug 级 precompress_phase_started/finished，覆盖 scan/read/staging/encode_write/publish/stdout 阶段，退出阶段记录 elapsed_ms；事件不包含资产路径或正文，错误返回也会退出阶段，不能据此判断写入成功。默认日志级别不输出这些事件。
- 预压缩 CLI 测试使用独立临时文件捕获输出，记录子进程 PID/总耗时；等待超过 120 秒会终止并回收直接子进程，报告各至多 1 MiB 输出。正常路径不再依赖等待子进程时的管道容量。这不是生产 deadline、输出文件容量上限或 OS 异常时的硬回收保证。
- 新增真实 CLI 测试解析 JSON，验证阶段成对、耗时非负、阶段集合、普通日志级别关闭，以及诊断不包含测试资产名/正文。serde_json 仅增加为宿主 dev-dependency，锁文件通过离线 Cargo 更新，不升级依赖版本。
- Windows 宿主 38 + precompress 6 项通过，Linux 宿主 38 + precompress 7 项通过；Windows 严格 Clippy、xtask check 通过。Linux 镜像 `ef5c520033c77dc7497f455616135e1d691afef2d75451e7a07ceb0bd718a781`。本次不是完整 workspace、受控负载或注入式超时回收验收。
- 本批增加诊断条件，未复现或解释上一批 62.89 秒延迟；PRECOMPRESS/EVENTS/SOAK 文档同步阶段语义和验证限制，QA-04 继续保留。

第二百批（2026-09-08，预压缩测试子进程超时故障注入）：

- 将捕获 helper 的超时结果结构化为 PID、实际耗时和 wait 后的 Output；普通调用仍使用原 120 秒门槛，故障测试可指定短截止。终止时处理 try_wait 与 kill 之间进程自行退出的竞态，不能把终止失败的存活子进程当作已回收。
- 新增自包含测试：以精确测试名重新启动同一测试二进制，在 fixture 分支输出 stdout/stderr 标记并等待；五秒后要求超时结果包含标记、非成功退出状态，Linux 进一步要求 SIGKILL。只有子进程 wait 返回后才形成结果，不增加 ignored 用例或依赖外部解释器/监听端口。
- Windows precompress 7 项、Linux 8 项通过；Windows 宿主严格 Clippy、xtask check 与 diff whitespace 检查通过。Linux 镜像 `a8845d69aacba167caaccd584874b922ca2dbfd3b66178a08509cbcf67080883`。此次为测试基础设施验收，未改生产压缩行为，未重跑完整 workspace。
- PRECOMPRESS/SOAK 同步故障注入证据。直接子进程回收不等同于后代进程树监管或异常 OS 状态下的硬时限，原 62.89 秒波动仍待受控负载定位。

第二百零一批（2026-09-08，相同配置恢复后的健康状态修复）：

- 先复现恢复问题：文件语法错误后写回与活动配置相同的内容，事务返回 NoOp，但宿主跳过激活，ReloadFailing 仍出现在健康报告中。
- owned reload 现在对所有带 generation 的成功结果（Started/Reloaded/NoOp）执行已有宿主协调。NoOp 仍不准备或替换 runtime、不消耗新 generation；恢复健康所需的宿主协调不再因计划相同而被跳过。
- 新增真实宿主回归依次覆盖语法错误、语义错误和完全相同内容恢复：拒绝时降级，NoOp 恢复后不再保留重载失败，Committed Arc 未替换且真实 HTTP 正文正确。
- Windows/Linux 宿主各 39 项与真实 run_reload 1 项通过，Windows 严格 Clippy、xtask check 和 diff whitespace 检查通过。Linux 镜像 `5f692516f819e9a8838d867c36ddb1d2ec48f2f526b930249eefce7d52b56c79`。本次不是完整 workspace 回归。
- 本次先修复确定性的串行恢复；迟到观察错误和激活期间新拒绝意图的健康状态归属仍需完整并发审计，不能仅用读取版本后的无锁上报冒充原子保证。ARCHITECTURE.md 记录此待办以及 NoOp 仍有宿主协调开销的边界。

第二百零二批（2026-09-08，事务状态作为配置健康的权威来源）：

- observe 层新增一次性安装的 ReloadStatus 接口，报告在 Health 锁外读取配置失败状态；App 通过 Weak<GenerationStore> 适配，避免 runtime/telemetry/store 强引用环。安装后 None 表示配置健康，不回退到迟到的推送失败；无适配器的独立使用保留原行为。
- 新增 reject_observation_if，在事务写锁内校验完整 Version/seal 后记录文件读取或解析失败；过期观察返回 Conflict 并由宿主映射 Stale，不影响新版本健康。失败不伪造来源树/修订，也不宣称阻止同一未变 Version 下已在途的准备。事务 Failure 增加饱和 attempts，legacy Health 计数也改为饱和。
- 三项新增回归覆盖过期/封闭观察拒绝、观察失败不改变来源修订；权威健康覆盖迟到通知且保持其他健康原因；以及旧激活被锁住期间新意图遭拒，释放后旧激活不会清除新失败。已有语法/语义错误后 NoOp 恢复回归继续通过。
- Windows/Linux observe 55、plan all-targets 208、宿主 40 与真实 run_reload 1 项通过，共各 304 项；Windows 三包严格 Clippy、xtask check 与 diff whitespace 检查通过。Linux 镜像 `914b2d8895f61f65947d304e4bd7ec4a3c6c832fd972b4896496000d6558362d`。非完整 workspace 回归。
- ARCHITECTURE.md 记录锁序、无 I/O/无递归回调、弱引用和采样边界。此改动解决配置降级依据的通知乱序，不是全系统原子健康快照，也未统一计数器/代号 gauge 的通知顺序；控制面授权、持久化恢复继续推进。

第二百零三批（2026-09-08，代号指标防止迟到通知倒退）：

- 先复现：第 2 代成功上报后，再上报第 1 代 Started/NoOp，原有 Gauge 被写回 1。generation 指标改用原子 u64 HighWaterMark 的 fetch_max，不再经过 i64 截断；Prometheus 类型仍为 gauge，HELP 改为已上报的最大成功代号。
- 新增逆序结果上报回归，验证代号维持 2 且 Started/Reloaded/NoOp 各计一次；新增多线程乱序写入及 u64::MAX 后迟到小值的底层测试。计数通知不去重，不把最大代号当成已经激活或当前事务读写版本。
- Windows/Linux observe 56、plan all-targets 209、宿主 40 与真实 run_reload 1 项通过，共各 306 项；Windows 三包严格 Clippy、xtask check 通过。Linux 初始镜像 `df6c3aad053da7fd3ea313a9c73f35fae07c61d99320c45e0b9f342e15299d7d`；最后仅修订 HELP 文字后，两平台另各重跑 7 项 render 测试通过，最终镜像 `c8a90a04d8c9826e9d10fd418da6f9d848379ffd1592155aa7e644149c8b3027`。
- METRICS/ARCHITECTURE 明确单 store/boot 归属、通知延迟、时间戳为通知处理时刻、下游浮点精度及非原子抓取边界。未执行完整 workspace 或性能基准；整体控制面与恢复任务仍待完成。

第二百零四批（2026-09-08，已激活 NoOp 避免重复生命周期协调）：

- 在已激活配置的同内容恢复测试中持有生命周期锁，先复现 NoOp 仍排队等待协调而超时。现在宿主维护私有 activated 代号，仅在完整协调与退休清理结束后以 Release 更新；NoOp 用 Acquire 判断是否还需要补激活，已激活代不重复排队。
- 配置健康由第二百零二批的事务权威来源恢复，不再需要为了清除旧推送失败而重复 activate。Started/Reloaded 仍保留原有任务所有权和激活流程，尚未激活的 NoOp 也不能跳过。
- 增强语法/语义错误后同内容恢复回归：生命周期锁仍被持有时 NoOp 可以完成、恢复健康、复用原 Committed 并正确提供 HTTP；新增低层只提交未激活的场景，随后 NoOp 必须激活，标记与真实 HTTP 一并验收。
- Windows/Linux 宿主各 41 项与真实 run_reload 1 项通过；Windows 严格 Clippy、xtask check、diff whitespace 检查通过。Linux 镜像 `04bdf92bae26e19e0386a2044762f7e2cbd87eb44222d4fc0edb81633ccb8ea4`。本次非完整 workspace 或性能基准。
- ARCHITECTURE.md 更新第二百零一批的临时全 NoOp 协调策略：仍执行来源观察、事务验证和指标上报，不宣称零开销或已测量延迟收益；激活代号不是新的外部管理接口。

第二百零五批（2026-09-08，宿主签发的单来源提交能力）：

- plan 公开 SourceWriter/SourceWriteError；只能由持有 store 的宿主为显式注册来源签发，开放式 legacy store、不匹配来源及已封闭 store 不签发。句柄固定来源、Registry 和宿主准备函数，调用方不能另选来源/准备函数或从句柄取得整个 store。
- 单次提交由完整 Version、可信前端计算的内容身份和节点组成，内部从最新已准入意图分配来源修订、保留其他来源，再走原条件事务。句柄以 Weak 持有 store；同版本并发写不会自动重试，source revision 耗尽或关闭/释放 store 拒绝继续准备。
- 四项回归覆盖来源命名空间越权拒绝且文件意图不变、拒绝后修复与 NoOp/删除 tombstone、同版本两线程只有一个成功且绑定准备函数只调用一次、严格签发与弱引用释放、修订耗尽/封闭不调用准备。
- Windows/Linux plan all-targets 各 213 项、宿主 41 项与真实 run_reload 1 项通过，共各 255 项；Windows 两包严格 Clippy、xtask check 和 diff whitespace 检查通过。Linux 镜像 `b9d45177e7fb367e4a71bd27e45afd4cc1fa6e98460fab9e1973a455977b3957`。本次不是完整 workspace 回归。
- ARCHITECTURE.md 明确这是同步计划层能力，不是远程 token、沙箱、请求限额、持久化或 App 激活接口；宿主须绑定可信准备逻辑、计算完整指纹而非相信客户端指纹、约束执行和过滤诊断。网络 Admin/持久化适配尚未接入，未用 API 定义代替产品功能完成。

## 1. 目标与范围

- 在现有 Snapshot → Spec → Plan → Generation / State 上继续建设，不启动第三次重写。
- 尽量恢复旧 main 的完整产品能力：每项功能必须有实现、可选包、迁移替代或明确待办，不用“常用 80%”代替完整迁移。
- 保留 v2 的静态文件、缓存、压缩、forward-auth、类型化配置、事务 reload、状态复用与诊断优势。
- 借鉴参考项目的插件公开边界、真实裁剪、组合约束和专项验收；实现遵循本项目 provenance 规则，不直接移植权属未确认的外部代码。
- 本轮规划不授权自动实施所有任务、切换分支、发布版本或执行系统信任修改。后续实施按任务批次推进。

已存在的基础不再重做：配置分层、Generation、LKG、基础 Registry、FromNode 派生宏、H1/H2、10 种选择器、手工 TLS、本地 CA、HTTP-01、基础观测及标准中间件。针对这些模块的任务是补齐契约与验证。

第二百零六批（2026-09-08，来源内容身份与配置树一致性）：

- `SourceSnapshot::observe` 对重复内容指纹执行完整节点身份校验；节点、参数、属性、顺序及来源位置任一变化返回 `ContentMismatch`，不分配 revision，不覆盖保留意图。浮点按位比较，相同 NaN 位模式可重试，正负零及不同 NaN 位模式不能冒充相同观察。
- 比较使用显式栈，不递归遍历；发生在来源观察阶段，不增加 generation 写锁内的树遍历。它仍有与输入大小相关的时间和内存开销，不是输入大小限制或远程请求沙箱。
- 单来源 `SourceWriter` 回归覆盖相同指纹提交不同 endpoint 节点：明确返回 Observation 错误，Version、活动 Committed Arc 和保留节点不变；同时覆盖 `Snapshot::observe_if` 的失败不修改语义。此前该 SourceWriter 断言已复现失败。
- 手动 `SourceSnapshot::new` 与底层完整 union 提交仍是受信任契约；不能宣称所有入口均会检测伪造内容身份。架构文档同时修正了“只保留指纹”的旧描述，实际保留最新快照及其树。
- 此前 Windows/Linux 验证句柄已不存在，未把丢失输出视作成功；本次重新执行 Windows 与 Linux 的 model/plan/KDL 全 targets，以及宿主 bin/run_reload，每平台 **339 项通过，0 失败**。Linux 使用镜像 `cfb8277b10f8023d0a68702c10b5215cdbb072d6d37edc849aa29f9160711302`。Windows 同范围严格 Clippy 与 `xtask check` 通过。这是定向验收，不是新一轮全工作区、性能或外部 CI 验收。

第二百零七批（2026-09-08，宿主单来源有界提交与激活）：

- 增加 `App::reload_source_if` 内部入口：只接收一个已观察的来源贡献，在 configuration worker 内捕获保留意图并校验 whole Version，合并后复用现有资源刷新、准备、二次 CAS 和 owned activation。与完整 union 提交共用总 reload 容量、串行配置工作槽和 shutdown drain。
- 不自动重试冲突，不读取文件、不修改 frontend observation cache；未提交的其他来源保留其最新意图，包括拒绝和 tombstone。这不是签发了来源权限的 SourceWriter，调用方仍需由宿主授权并正确分配 revision/content；目前仅为内部适配入口，没有暴露网络 API，也没有持久化承诺。
- 扩展真实 HTTP 来源组合测试：单来源增添上游、旧 Version 冲突且活动 Arc 不变、拒绝配置保留、空节点删除、关闭后 Stale。扩展排队测试，完整 union 与单来源均在排队期间版本改变后 Conflict，且不读取文件或激活监听；容量耗尽也覆盖单来源入口。
- Windows/Linux 宿主 bin 41 项及 run_reload 1 项分别通过；Windows 严格 Clippy 通过。Linux 镜像 `cc3d1245d89b50eed9577f927058dfab86912d92fb9c2c5ad6583132313f6097`。沿用共享生命周期的测试，不宣称单来源取消/外部 Secret 轮换的独立全组合验收或 Admin API 完成。

第二百零八批（2026-09-08，绑定来源的宿主异步写入句柄）：

- 新增 `AppSourceWriter`，仅由宿主向明确注册且未关闭的来源签发。句柄绑定 Weak App 和 SourceId；提交者不能选择来源、revision、Registry 或 prepare 回调。观察/修订分配、内容身份检查在受限 configuration worker 内进行，随后复用资源刷新、事务发布及 owned activation。
- `reload_task` 泛化错误类型以保留 Conflict/Observation 分类，不改变容量或生命周期语义。旧的受信任单来源入口迁至 `app_writer.rs`，主 App 文件回到 600 行目标以内；plan 层共用只读的显式来源注册查询，查询结果本身不构成授权。
- 新增三项宿主回归：有真实 listener 的来源提交、自动修订、NoOp、指纹复用拒绝、旧版本冲突、tombstone 和 HTTP 仍可服务；未注册/关闭/释放后的句柄不可用且 Weak 不保活；取消八个等待方仍占用总容量、溢出拒绝、已接收工作继续并排空、同一旧 Version 不被自动重试。
- 测试初始夹具缺少必需 global listener 导致 Rejected，补齐真实监听配置后通过；没有放宽生产验证规则。句柄不等同于网络认证、输入大小限制或持久化 API，暂未由 CLI/HTTP 对外分发。
- Windows 与 Linux：plan 127 + 6 + 81 项、宿主 bin 44 项和 run_reload 1 项，共每平台 **259 项通过**。Windows 严格 Clippy、`xtask check` 通过；Linux 镜像 `45568a958a3bfaf4d58c887a0e674d50bcea8e770a13086f209edd45aea5c788`。这是定向验证，不代表新一轮完整工作区或外部部署验收。

第二百零九批（2026-09-08，绑定来源的有界 KDL 文本入口）：

- 新增 `AppSourceWriter::reload_kdl_if`：调用方提交文本而不是任意 fingerprint/Node；宿主按固定格式域、绑定来源标签与原文计算身份，统一 origin，仍在有界配置工作线程内完成解析、观察、准备和激活。原有格式无关节点入口保留给受信任 frontend。
- 文本上限 1 MiB、节点上限 10,000，复用 KDL 解析前深度保护；所有层级的 import 明确拒绝，不执行文件加载。节点计数发生在解析之后，且 String 到达入口时已分配；这不是网络流式限额、CPU deadline 或进程内存硬上限，HTTP 层仍需前置认证和 body 限额。
- 语法、字节/节点限额和 import 错误通过 conditional observation rejection 记录健康失败，不改变 Version、保留意图或活动 Arc；旧 Version 在解析前 Conflict，不污染当前健康。重复有效原文走 NoOp 并恢复健康，未把不完整语法树当作新来源修订。
- 新增边界测试覆盖恰好/超过字节及节点限额、深嵌套、顶层/嵌套 import、来源与原文身份；扩展宿主真实监听测试覆盖文本提交、失败不发布、NoOp 恢复和过期预条件。控制面网络服务与持久化仍未完成。
- Windows/Linux 宿主 bin 46 项和 run_reload 1 项各通过；Windows 严格 Clippy、`xtask check` 与 `git diff --check` 通过。Linux 镜像 `7af7b90d0c1567bdfc6c5ccfaa6870a9bff7f5c28d41a27850b734fba6cdc6fe`。不是完整工作区、HTTP 管理客户端或外部部署验收。

第二百一十批（2026-09-08，可选管理 HTTP 处理器）：

- 新增默认关闭的 `admin-api` feature 和内部 `AdminApi`：宿主随机 token 授权、固定摘要常量时间比较、绑定 admin writer、版本 ETag 查询及 If-Match KDL PUT。新增直接依赖均为锁文件已有版本；没有更改 certon 版本。
- 认证、Origin 拒绝、请求容量、条件头和格式校验先于 body；八个请求容量、1 MiB 流式累计、5 秒 body 时限、frame 总数限制；拒绝 trailers、非法 UTF-8、重复/异常长度与 CL/TE 歧义。所有响应 no-store，外部错误固定摘要，不暴露配置诊断或 Secret。
- 五项处理器测试覆盖认证/条件/容量失败不读取 body，实际 App 配置发布和健康恢复，流式超限/非法输入，虚拟时钟慢 body，读取期间其他请求先提交后旧请求 412 且活动 Arc 不变。
- Windows/Linux 宿主 bin 51 项和 run_reload 1 项各通过，最小 features 构建检查通过；Windows 严格 Clippy 通过。此时 Linux 镜像 `a262325290a33f4891a5927c113b68161d459870fca67c1a19633e0b4b8cccd1`，随后另行复验长度/framing 加固。
- 新增 `docs/ADMIN.md` 明确接口与限制。这是 HTTP 对象处理器，不是可启动的管理网络服务；没有默认开放端口/CLI 参数。独立监听、header/连接限制、TLS、shutdown body 排空、真实网络 framing、持久化与崩溃恢复仍待实现，CTL-02/03 不标完成。
- 最终长度/framing 加固后 Windows/Linux 的五项 admin 测试再次通过；Linux 最终镜像 `d89e5a3a582108ff360abaf5d5bcecbbdf87658f061e31fd8293985e070b4be1`。Windows 严格 Clippy、`xtask check`、`git diff --check` 通过；没有把定向复验额外计入独立测试总数。

第二百一十一批（2026-09-08，真实 loopback 管理监听与网络排空）：

- 新增显式 `AdminServer::start`，仅接受 loopback socket；拒绝 wildcard/非 loopback 明文地址。默认 feature 仍不打开任何端口，CLI 接线尚待完成。
- HTTP/1 禁用 keep-alive，限制 32 个连接、32 个 headers、16 KiB 解析 buffer、5 秒 header 读取时限；连接满额直接关闭新 socket，不创建额外响应任务。连接任务与完成回收由一个 JoinSet 管理。
- shutdown 关闭 listener、广播取消、等待所有连接退出；覆盖未完整 headers、已经认证但未读取完的 body。Drop 仅发送异步停止信号。已经交给 App 的配置事务仍由 App 拥有，需要宿主随后调用 App shutdown，不把网络取消当事务回滚。
- 五项真实 TCP 测试覆盖 401、ETag 查询/PUT、chunked 超限，32 连接满额，header 数量/大小与虚拟时钟 header 超时，网络 shutdown/Drop 与 wildcard 拒绝。依赖仅增加已有 workspace Hyper/Hyper-util 的 optional 直接引用。
- Windows/Linux 宿主 bin 56 项、run_reload 1 项各通过，最小 features 构建检查通过；Windows 严格 Clippy 与 `xtask check` 通过。Linux 镜像 `ce69557f993a5d6e2cddca4b02a8c34f233f34bf6e98ce216a5b9c1383318ddd`。不是全工作区、远程 TLS、真实外部客户端或攻击长稳验收。
- 更新 ADMIN/ARCHITECTURE，明确接下来的 CLI 私有 token 配置、生命周期顺序、监听健康/故障传播及持久化边界；CTL-02/03 不标完成。

第二百一十二批（2026-09-08，管理监听 CLI 与退出顺序接线）：

- `run` 在 `admin-api` 构建中新增成对的 `--admin-listen` / `--admin-token-file`，仅显式启用时注册 admin 来源；默认运行与静态 serve 不启用管理监听。token 复用 Secret 文件有界读取、Unix 权限检查及末尾换行处理，再验证 token 长度/字符；Windows ACL 由部署者负责，不伪称已验证。
- 启动先校验 token/保留 loopback socket，初始业务配置成功后启动管理接受循环。dry-run 关闭管理监听后排空 App；正常信号也先关闭管理连接再排空配置事务。管理任务意外结束会触发全进程排空并返回 3；JoinHandle 结果只消费一次，取消等待不会丢失句柄。
- 新增三个配置/凭据单测和一个真实 CLI 测试：参数成对/拒绝 inline token、非 loopback 在读文件前拒绝、私有权限/换行/非法内容、管理 PUT 与文件 watcher 共存及日志不包含 token。Linux CLI 使用 SIGTERM 验证成功退出；Windows 仍以 kill 收尾，不宣称其控制台信号验收完成。
- Windows/Linux 宿主 bin 59 项、run_reload 2 项各通过，最小 features 检查通过；Windows 严格 Clippy、`xtask check` 和 diff 检查通过。Linux 最终镜像 `40d4e255aed62216293f8ed290c289e5e1b66f306a94780ccf435c3ec72de226`。门禁曾指出两个嵌套 if 与未登记事件，已按规则修正，没有放宽门禁。
- 新增 `admin_serving` 事件并更新 ADMIN/CLI/ARCHITECTURE 使用说明。尚缺持久化/崩溃恢复、token 热轮换、远程 TLS 和长稳攻击验收；CTL-02/03 仍不整体标完成。

第二百一十三批（2026-09-08，管理 CLI 接入后的顺序全工作区回归）：

- Windows 随后 Linux 顺序执行 `cargo test --workspace --all-features --all-targets --locked --quiet`，不中途重启：Windows **1360 通过、0 失败、4 忽略、100 组**；Linux **1375 通过、0 失败、4 忽略、100 组**，均 exit 0。基础 HEAD 为 v2 `6ca3741`，包含工作区未提交改动；Linux 全量同步源码后的镜像为 `832b8ac6042b256d4cd67a215df7d2980e5b58b67655385117128d6e2027e907`。
- 四项忽略未执行、未计为通过：两处真实 PHP-FPM 互操作、PHP 密码互操作和显式 soak。没有据此宣称 macOS/MSRV/远端 CI、外部 CA、长稳/性能、发布或旧功能完整化完成。
- Linux 超过 60 秒提示：`named_file_sources_publish_reload_and_keep_lkg_on_failed_observation` 所在宿主组耗时 **77.42 秒**；`invalid_utf8_syntax_excessive_nesting_and_size_have_no_partial_output` 所在 format 组 **88.74 秒**。没有失败，但不能视为稳定性能通过。一次 docker top 仅观察到 format 测试进程处于 futex 等待，不能定位子进程、解析、磁盘或调度根因。
- 完整运行结束后，同镜像独立复测两项分别 **5.67 秒、0.18 秒**通过。复测使用 `-p gatel` 且发生重编译，不是相同全工作区 feature 合并/运行压力下的严格性能对照。环境另有构建进程，未停止它们，也未把资源竞争确定为根因。
- QA-04 新增待办：给 format CLI 子进程等待与 watcher 测试阶段补充有界等待/阶段证据，在受控负载下复现；第 181/198 批历史暂态与慢运行问题不因本批全绿关闭。当前 Linux precompress 八项组为 5.00 秒（含有意五秒超时夹具），不能证明旧预压缩延迟已修复。
- Windows 全工作区严格 Clippy、`xtask check`、`git diff --check` 通过。本批未改生产代码或放宽测试，只推进跨功能验收与待调查项的证据记录。

第二百一十四批（2026-09-08，格式化 CLI 子进程有界等待与诊断）：

- 将既有 precompress 专用执行器移动并解耦为 `tests/support/cli_process.rs`，使用独立唯一临时目录，不依赖调用测试的 Directory 类型。format 的所有 fmt/validate 调用改用该执行器，不再无界等待普通 Command::output；precompress 继续使用同一回收与超时协议。
- 每次子进程记录 PID、spawn 返回时耗时、完成时总耗时；format 额外记录固定输入类别编号/字节数、fmt/check/validate 命令与换行类别，不打印配置正文。默认测试捕获这些输出，定位时用 `--nocapture` 显示；失败报告包括捕获的 stdout/stderr。
- 120 秒等待限额后终止并 wait 回收直接子进程。它不能强制中断 OS spawn、文件 I/O 或后代进程树，也不是生产 CLI 的 deadline。捕获输出改为读取 1 MiB + 1 检测超限并明确失败，不再以静默截断的 Output 参与断言；磁盘写出仍不是硬容量限制。
- 新增共享执行器真实子进程输出超限回归（在 format/precompress 两个测试二进制内各执行一次），并保留五秒永久等待 fixture 的终止/回收验证。Windows format 6 + precompress 8 项通过，Linux 6 + 9 项通过；Windows 严格 Clippy、`xtask check` 通过。Linux 镜像 `550a2ada5e8ec4cddc07b1f4a209112dd6c87903ca6e53193b812b6313d8b79b`。
- 本批增加下一次复现所需证据，不证明已找到或修复此前 88.74 秒 format 慢运行原因；watcher 阶段定位、受控压力与全量复跑仍待继续。

第二百一十五批（2026-09-08，命名来源 watcher 阶段证据与关闭期限）：

- 为此前 Linux 慢运行的命名来源 watcher 测试增加独立 `app_test_phase.rs`：同步 attempt/文件写入和各 async 阶段记录开始、scope 结束壁钟耗时与 unwind 标记；包含 setup、HTTP、watch/import 更新、join、shutdown、文件清理。仅测试代码，不新增生产日志或配置行为。
- 既有 HTTP 3 秒与 watch/import 12 秒断言不放宽；activation、watcher join、App shutdown 新增 20 秒协作式等待期限。虚拟时钟 pending future 回归验证带阶段名的超时失败，不借此宣称可终止同步 I/O、阻塞线程或 runtime 析构。
- Windows 单项带输出复测总计约 5.77 秒，watch.reload 为 5.263 秒，两个 import 更新各约 0.24 秒；本次 activation/shutdown 很短。可定位本次主要耗时与五秒 fallback 一致，但不能解释或关闭历史 77.42 秒异常。
- Windows/Linux 宿主 bin 60 项 + run_reload 2 项各通过；Windows 严格 Clippy、`xtask check` 通过。Linux 镜像 `b28c0d466a03b422b79471b23e6d339283117b5779b6c79290d41427f8a2c735`；全工作区和受控负载复跑仍待继续，QA-04 不标完成。

第二百一十六批（2026-09-08，管理监听监督取消与结果消费验收）：

- 增加三个回归，覆盖 CLI 使用的监督/关闭边界：取消尚未完成的 terminated 等待后，原监听句柄保留且真实 HTTP 查询继续成功；监督已消费正常完成结果后再 shutdown 不二次轮询 JoinHandle；取消 shutdown 等待后，独立拥有的网络任务仍关闭真实连接并归还全部连接槽。
- 注入监听任务 Err 验证错误回执仅返回一次，调用方保留原错误后可继续 shutdown；并非实际 OS accept 失败或真实 CLI 故障退出注入，不据此扩大结论。补充方法及 ADMIN 文档，明确“取消安全”不等于能丢弃已收到错误而仍从第二次调用取回它。
- Windows/Linux 宿主 bin 63 项 + run_reload 2 项各通过；Windows 严格 Clippy、`xtask check` 通过。Linux 镜像 `cc4fe158a4bfbcef08c30acd64b0b8445a1bec369c65324ca90dcae58fb776d3`。未发现需要修改的生产运行逻辑，本批为已实现生命周期契约补充直接证据；全工作区/长稳及持久化任务仍待继续。

第二百一十七批（2026-09-08，Admin 持久化记录与恢复扫描基础）：

- 新增未接入生产写入的 `admin_journal.rs`：版本化记录、非零 u64 顺序号、正文/头部独立校验、UTF-8 与 1 MiB 正文/16 MiB 扫描上限。先验证完整头部再相信长度，防止长度位损坏被误判成可忽略短尾。
- 扫描仅返回最新完整原文、连续顺序、有效前缀位置和短尾标记，不自动改文件；None 与空贡献分别表示没有完整记录和 tombstone。完整损坏、非法 UTF-8、跳号/重复/倒退/溢出明确失败，不返回部分成功。Debug 不输出原文，校验值不冒充 MAC 或文件授权。
- 五项回归覆盖所有第二条记录截断位置、每个完整记录字节翻转、magic/校验/长度异常、顺序边界、中文、空配置和大小限制；这是字节级测试，不是进程崩溃注入。
- 新增 `docs/design/admin-journal.md`，明确尚需解决的 LKG/checkpoint、独占写锁、日志实例身份、磁盘同步与发布顺序、结果未知/幂等回执、启动恢复、压缩和所有崩溃窗口。仅恢复最新意图不等于恢复最后成功服务配置，不能据此称 CTL-03 已完成。
- Windows/Linux 宿主 bin 68 项 + run_reload 2 项各通过；Windows 严格 Clippy、`xtask check` 通过。Linux 镜像 `32adc96a40378bcca91b13d23427442db47e20661f1e15750f3f89eece7f08c9`。HTTP 200 仍为内存应用，没有新增 journal CLI 开关或磁盘写入承诺。

第二百一十八批（2026-09-08，独占追加日志磁盘基础）：

- 新增未接到请求/CLI 的 `admin_journal_store.rs`：已有私有目录、稳定保留的 writer.lock、OS 独占锁、普通文件/权限检查、有界读取和完整日志恢复。文件与 Unix 父目录同步，不冒充 Windows 目录同步或任意存储设备的断电保证。
- 追加完成 write_all + sync_all 后才返回顺序号；可能改变文件后的错误使该句柄停止写入，纯输入/容量错误不污染。短尾保留但拒绝追加，损坏/超限拒绝，不自动截断、回退或覆盖；从零恢复，checkpoint 和日志压缩尚未实现。
- 回归覆盖排他打开、空贡献与重开、部分写入后 poisoned/短尾保留、模拟 sync 失败可能仍留下完整记录、损坏/非普通/超大文件、非法输入不污染；Linux 另覆盖私有目录/文件权限与符号链接拒绝。错误注入不是 OS 断电或真实跨进程崩溃证明。
- Windows 宿主 bin 73 项 + run_reload 2 项通过；Linux 74 + 2 项通过。Windows 严格 Clippy、`xtask check` 通过；Linux 镜像 `3fb748d19fd8fe91fb09617f0434634fc39b68c85ab328b4842bbc798220805e`。初始测试断言的 io::Error 不支持 PartialEq 和跨平台 unused_mut 已修正。
- 更新持久化设计与 ADMIN 边界。仍需 LKG/成功回执状态机、提交顺序、启动恢复、实例绑定、跨进程故障注入；HTTP 200 的内存语义未改变，CTL-03 不标完成。

第二百一十九批（2026-09-08，日志跨进程排他与强制终止恢复）：

- 新增真实子进程回归，复用当前测试二进制运行两种 fixture：同步完整首条记录后持锁等待；再同步第二条记录的部分字节后持锁等待。父进程确认第二次打开确实因写锁被拒绝，再强制 kill 并 wait。
- 子进程结束后可重新取得 OS 锁。完整日志恢复首条并成功追加下一顺序；短尾恢复首条但拒绝继续追加，文件字节完全保留。Linux 检查 SIGKILL；等待 fixture 就绪有 10 秒期限，异常退出或超时提供 PID/模式/日志证据，Drop 终止并回收残留直接子进程。
- Windows 宿主 bin 74 项 + run_reload 2 项通过；Linux 75 + 2 项通过。Windows 严格 Clippy、`xtask check` 通过；Linux 镜像 `be526f7b1105c56d0db1bd89340ef9df6d378c4c3dcedb75a73b0924e96b5fef`。
- 测试固定同步的部分字节以检验恢复分类，不能模拟真实 torn sector、断电、未刷盘数据丢失或配置发布/HTTP 回执窗口。未修改生产行为，补充磁盘层基础协议证据；CTL-03 与 HTTP 持久化承诺仍未完成。

## 2. 旧功能恢复台账

“已有”指当前源码及功能清单有实现证据，仍需按旧行为做兼容测试；不代表生产验收完成。

| 旧 main 能力 | 当前 v2 | 后续方向 / 任务 |
|---|---|---|
| H1/H2、反向代理、WebSocket、10 种均衡、主动/被动健康 | 已有 | 协议、超时、取消、复用专项验证，RT-01～04 |
| Host/Path/Method 与复合匹配器 | 已有部分更丰富匹配 | 清点旧正则、组合语义和默认值，BASE-01、WEB-04 |
| DNS / SRV / Unix 上游 | 系统 DNS、Unix 后端已有；SRV 未恢复 | SYS resolver 保留，TTL/DNS/SRV 可选发现器，NET-01～02 |
| HTTP/3 | 未恢复 | 可选 transport，NET-04 |
| TCP 与 UDP stream proxy | 未恢复 | 独立 L4 包与会话生命周期，NET-05 |
| CGI / FastCGI / SCGI | FastCGI/SCGI 可选 terminal 和 Windows 子进程 CGI 已实现；Linux/macOS 子进程 CGI 待完成 | 协议、H2 取消、Windows 真实进程及热更新已有测试；继续跨平台/真实应用互操作，CGI-01～04；边界见 CGI.md |
| Forward proxy / CONNECT | 未恢复 | 可选包、认证与出站策略，NET-06 |
| PROXY protocol、可信客户端地址 | 已有 | 检查 v1/v2、IPv4/IPv6、头信任边界，RT-04 |
| 静态文件、Range、预压缩、目录浏览 | 已实现；目录浏览默认关闭且有 HTTP 回归 | 继续旧行为差异与组合验收，WEB-01；file_listing/file_variants/file_conditions 和 serve 测试提供当前证据 |
| cache / compress / decompress | cache、encode 已有；可选 decode 已实现受限请求解压 | 继续独立互操作、压力和流式/缓存组合验收，WEB-02～03 |
| auth / forward-auth / IP / rate-limit / CORS / timeout | 已有主要能力 | 多哈希、额度及安全组合，AUTH-01、EXT-02、RT-05 |
| rewrite / redirect / replace / stream-replace / templates / placeholders | 部分已有；模板、替换及旧占位符仍缺 | 可选内容包、受限变量与迁移说明，WEB-04～05 |
| request/response header 操作 | 已有基础 headers | 核对 header-up/down、替换和占位符，WEB-04 |
| error pages / body buffer | 错误页与显式重试缓冲已有 | 不等价于完整旧 buffer 指令，WEB-06 |
| Circuit breaker | 被动摘除已有，独立熔断器未恢复 | 区分 endpoint 摘除与 upstream 熔断，RT-06 |
| Manual TLS / mTLS / ACME / local CA | 部分已恢复 | TLS-01～04 补齐挑战、按需签发与 trust/untrust |
| Admin / runtime services / persistence / drain | 可选 KDL Admin、多来源 CAS、结构化状态、检查点恢复及离线压缩已有；旧资源 API、完整 WAL/幂等回执及资源级 drain 未完成 | 继续 CTL-01～04；不可把检查点模式等同于完整持久化协议，见 ADMIN.md/CLI.md |
| Prometheus / 日志 / OTLP | metrics、日志已有；OTLP 未恢复 | OBS-01～02 |
| serve / reload / completions / manpage / passwd / precompress | serve、watcher reload、补全、手册、passwd --stdin、precompress 已有实现与 CLI 回归 | 独立旧 reload 命令、交互密码输入及完整参数兼容仍逐项核对，OPS-01；见 CLI.md/compat-v1.md |
| Windows Service / systemd / 安装包 | 发行配置已有一部分，实际验证不足 | OPS-02～03 |
| fuzz / smoke / dockertest / benchmarks | 已有新测试及工程配置 | 恢复场景覆盖，执行跨平台与互操作验证，QA-01～04 |

BASE-01 必须逐条检查旧 `docs/en`、`examples`、CLI、config parser 和默认值；本表是入口，不能仅按模块名宣布兼容。

## 3. 架构增量决策

### 3.1 可选能力包

插件先采用可信任、源码链接的 Rust crate。关闭 feature 必须同时移除依赖、factory 注册与能力声明。缺少插件的配置在编译期报错，不能静默回退。包名是建议，实施时以依赖方向为准。

| 包 / 宿主 feature | 默认 | 内容与边界 |
|---|---|---|
| 现有标准 HTTP bundle | 保持现状 | 常见 HTTP 功能；不立即拆散每个轻量指令 |
| `gatel-fastcgi` / `fastcgi` | 关闭 | PHP-FPM 等外部 FastCGI；不管理 PHP 进程 |
| `gatel-scgi` / `scgi` | 关闭 | SCGI 外部服务 |
| `gatel-cgi` / `cgi` | 关闭 | 受控启动子进程；不允许请求文本拼成 shell 命令 |
| `gatel-forward-proxy` / `forward-proxy` | 关闭 | 认证后的 HTTP/CONNECT 与出站准入 |
| `gatel-l4` / `l4` | 关闭 | TCP/UDP，复用 endpoint/health/selector，而非 HTTP sender |
| HTTP/3 transport / `http3` | 初期关闭 | QUIC、H3、TLS、Alt-Svc，与现有 Generation 协作 |
| `gatel-decode` / `decode` | 关闭 | 已实现受限请求解压；独立于响应内容扩展 |
| 内容扩展 / `content-extras` | 关闭 | templates、replace、stream-replace；按依赖体积再拆 |
| DNS discovery / `dns-discovery` | 关闭 | TTL、SRV、地址准入；保留系统 resolver 为默认选项 |
| OTLP exporter / `otlp` | 关闭 | traces / metrics 导出，不拥有业务 Listener |
| Admin / `admin` | 关闭 | 配置来源、资源 API、持久化、审计 |
| ACME、本地 CA、多密码哈希 | 保持已有默认；新算法显式开 | 逐步验证独立裁剪，不在迁移期间暗改默认 TLS 行为 |

内核保留发布事务、请求身份、取消、body 生命周期、资源准入和状态所有权。不要把这些正确性责任复制进每个插件。也不要让所有协议共用一个万能 `Transport` trait：先用 FastCGI、SCGI、TCP 的真实需求识别可共享部分。

### 3.2 在 phase 之上建立效果约束

保留现有四个 phase，新增小型、类型化 `Effects` 声明，覆盖：读取/消耗/替换 body、认证与身份、上游调用、额度消费、预检短路、响应头所有权、压缩、缓存、Upgrade、trailers。

编译器结合固定安全偏序、scope 与用户 order 建立执行顺序；用户 order 只能在安全约束内排序。矛盾或不可满足的组合给出带来源诊断；`explain` 输出顺序及原因。第一版只覆盖已有指令和少量必需效果，不做通用策略语言。

必须明确：CORS 预检允许跳过哪些业务副作用；未知长度超限时已转发前缀如何处理；认证信息是否可转发；缓存命中是否执行认证和限流；压缩/替换后的 Content-Length、ETag、Vary、Range 如何变化。固定 phase 本身不能证明这些组合正确。

### 3.3 资源句柄与状态继承

在现有 BuildContext、StateBinding、Upstreams 接口上增量引入按声明授予的句柄：文件根、Secret、出站目标、子进程、后台任务。禁止插件绕开宿主生命周期；可信进程内 Rust 代码仍可自行 I/O，因此这是受支持接口与审计边界，不是安全沙箱。

保留资源级兼容指纹，不把所有复用判断压入一个全局 plan hash。增加 Preserve / Reconfigure / Replace 决策说明；活动共享状态不得在 prepare 中更新，新策略在 commit 时发布。旧连接使用旧策略还是读取新的共享限制必须逐种状态写清楚。

普通路由修改继续固定连接 Generation；权限撤销、紧急封禁、凭据撤销另定义显式撤销/受控 drain 策略，避免操作者误以为 reload 会立即更新旧 H2 stream 的权限。

### 3.4 多来源与配置版本

完成现有 SourceId/RevVector 基础上的资源所有权、跨来源引用、删除约束与 source revision CAS。Admin 与文件必须走同一编译/prepare/commit 路径。提交内存状态与持久化之间定义明确恢复协议，不能把“写 JSON + swap”称为原子持久事务。

配置兼容优先提供 `adapt --from v1` 与行为差异说明。历史上不安全或含糊的默认值不盲目恢复：标注 breaking change，提供显式配置路径和回归场景。

## 4. 可执行任务列表

所有任务初始状态均为 TODO。每项完成时补充提交、测试命令、平台及证据路径，同时更新 FEATURES。优先级 P0 为基础门禁，P1 为本轮完整化必做，P2 为增强；不是省略 P1 的理由。

| ID | 优先级 | 任务 / 产出 | 依赖 | 验收条件 |
|---|---|---|---|---|
| BASE-01 | P0 | 建立 `docs/compat-v1.md`，旧功能/参数/默认值/示例/CLI 全量映射 | 无 | 每项有 v2 状态、任务 ID、兼容或 breaking 说明；包含 TCP 和 UDP |
| BASE-02 | P0 | 固定 v2 基线与性能测量方法 | 无 | 保存机器、构建、请求集、吞吐/延迟/RSS/FD 数据，至少重复 3 次 |
| BASE-03 | P0 | 独立增量 ADR：效果约束、资源句柄、插件边界、状态迁移 | BASE-01 | 每条不变量对应测试计划；保留来源说明；不过度抽象 |
| EXT-01 | P0 | 公共插件 API 分层、稳定 ID、实现版本/指纹契约 | BASE-03 | 区分稳定纯契约、同版本执行桥、内部实现；声明能力与 executable factory 一致 |
| EXT-02 | P0 | Effects 与约束编译、可解释排序 | EXT-01 | CORS/auth/限流/body-limit/cache/encode/retry 的合法及冲突组合测试；order 无法绕开约束 |
| EXT-03 | P0 | 声明式资源句柄、prepare 回滚、受监督任务 | EXT-01 | 文件/Secret/出站/进程权限按声明获得；候选失败无任务或资源残留 |
| EXT-04 | P1 | 包级 feature、默认预设、外部插件夹具 | EXT-01 | 外部 workspace 插件可执行、reload、LKG；关闭包后依赖树和 manifest 真消失 |
| EXT-05 | P1 | FromNode / schema / 文档工具完善 | EXT-01 | 从同一类型生成字段帮助；保留手写 decoder；schema 不泄露 Secret |
| RT-01 | P0 | 审计 prepare/commit/state 更新原子性与 stale 候选 | BASE-03 | 故障/并发注入证明失败候选不改共享状态；无重复 activate、代际混合 |
| RT-02 | P0 | H1/H2 pool、取消、GOAWAY、队列容量专项验证 | RT-01 | 取消归还 lease/reservation；真实 H2 多路复用；期限涵盖 DNS/TCP/TLS/HTTP 握手总和 |
| RT-03 | P0 | Retry 状态机与重放语义审计 | RT-02 | pre-send 证明明确；replay 显式幂等授权、body 可重放、流量预算；5xx 不默认重放 |
| RT-04 | P0 | 请求身份、hop-by-hop、PROXY、framing/trailers/Upgrade 审计 | EXT-02 | 伪造转发头、重复 framing、半关闭、H1/H2 桥接有 wire-level 测试 |
| RT-05 | P1 | 状态 Reconfigure 与紧急撤销策略 | RT-01,EXT-02 | 调限额保留使用量；旧连接策略有契约；删除重建、拒绝候选、并发更新均测试 |
| RT-06 | P1 | Upstream 熔断器，可选指令接口 | RT-03,EXT-03 | closed/open/half-open、probe 预算、取消、reload；与 endpoint 摘除无重复计罚 |
| NET-01 | P1 | Resolver Provider：系统与 TTL DNS 两种明确策略 | EXT-03,RT-02 | hosts/split-DNS 行为有说明；TTL/stale/负缓存/地址准入测试；实际连接不二次解析绕过检查 |
| NET-02 | P1 | SRV 与受控 HTTP JSON discovery | NET-01 | 权重/优先级、稳定 endpoint ID、部分失败、自回环、集合更新不误重置状态 |
| NET-03 | P1 | Unix socket Listener | RT-01 | 路径所有权、权限、重启残留与关闭清理；不删除不属于本进程的 socket |
| NET-04 | P1 | 可选 HTTP/3 transport | EXT-04,RT-04,TLS-01 | H3/TLS/Alt-Svc、reload、QUIC drain、UDP 资源上限；真实 H3 客户端验证 |
| NET-05 | P1 | 可选 TCP/UDP stream 包 | EXT-03,EXT-04,RT-05 | TCP 半关闭/背压；UDP 会话、超时、容量和回包映射；reload/drain 无泄漏 |
| NET-06 | P1 | 可选 Forward Proxy / CONNECT 包 | EXT-02,EXT-03,RT-04 | 默认认证、目标端口/地址准入、DNS 安全；隧道预算与关闭；禁止无意公开代理 |
| CGI-01 | P1 | 定义非 HTTP terminal 的流、环境与生命周期契约 | EXT-02,EXT-03 | 不套用 HTTP 连接池；明确 body/headers/stderr/取消；FastCGI 与 SCGI 样例证明最小接口 |
| CGI-02 | P1 | `gatel-fastcgi`，优先 PHP-FPM | CGI-01,EXT-04 | TCP/Unix、SCRIPT_FILENAME/PATH_INFO、记录分片、stdout/stderr、超时；真实 PHP-FPM 与路径穿越测试 |
| CGI-03 | P1 | `gatel-scgi` | CGI-01,EXT-04 | netstring 环境、CONTENT_LENGTH、未知 body 长度策略、流式响应和取消；真实或独立协议后端 |
| CGI-04 | P1 | `gatel-cgi` 子进程 terminal | CGI-01,EXT-04 | 可执行文件/root/env 白名单、进程并发、stderr 限额、超时杀进程树并回收；无 shell 插值 |
| WEB-01 | P1 | 静态文件兼容与目录浏览补齐 | BASE-01,EXT-03 | 索引/隐藏文件/symlink/containment/Range/预压缩/HEAD；目录浏览显式开启 |
| WEB-02 | P1 | decode 与 encode/缓存/大小限制联合契约 | EXT-02 | 解压后容量与膨胀预算、流式取消；Vary/ETag/Content-Length 正确 |
| WEB-03 | P1 | 缓存旧行为核对与安全补齐 | EXT-02,RT-05 | Authorization/Cookie/Vary/no-store/HEAD/失效、配额；明确缓存命中是否扣额度 |
| WEB-04 | P1 | rewrite/matcher/header-up/down/占位符兼容 | BASE-01,EXT-02 | 正则与复合语义、编码、大小写、头保护；变量采用受限类型化表达，不随意引入脚本引擎 |
| WEB-05 | P1 | 可选 templates、replace、stream-replace | WEB-02,WEB-04,EXT-04 | 跨 chunk 匹配、编码/膨胀边界、文件访问、取消；模板访问能力受限 |
| WEB-06 | P1 | buffer / error-pages 旧语义核对 | EXT-02,WEB-02 | 显式内存/磁盘预算、未知长度策略、临时文件清理；已发送响应不尝试第二次写状态 |
| WEB-07 | P2 | 缓存重验证、请求合并及淘汰维护 | WEB-03 | 304、并发首请求、慢源、取消和错误不污染缓存；独立性能验证 |
| TLS-01 | P0 | ACME staging 与证书恢复真实验收 | RT-01 | 授权测试域签发/重启/续期失败场景；缺外部条件时标为待外部验证，不伪造完成 |
| TLS-02 | P1 | TLS-ALPN-01 / DNS-01 provider | TLS-01,EXT-03 | listener challenge 路由、DNS Secret/清理、超时与取消；供应商有独立适配 |
| TLS-03 | P1 | 受控 On-Demand TLS | TLS-01,EXT-03 | allowlist/ask、总域名数/并发/速率/退避、重启恢复；不在 prepare 阻塞签发 |
| TLS-04 | P1 | local CA / trust / untrust、mTLS 兼容与撤销边界 | BASE-01,RT-05 | 平台独立证据；信任库变更需明确授权；证书续期与身份/策略改变分开处理 |
| AUTH-01 | P1 | Argon2/scrypt/PBKDF2 可选算法及凭据轮换 | EXT-03,EXT-04,RT-05 | 算法成本/并发有界；Secret 不泄漏；失败轮换保留 LKG；认证头转发策略明确 |
| CTL-01 | P1 | 多来源资源所有权、引用与 revision CAS | RT-01,BASE-03 | 文件与 runtime 并发写不覆盖；冲突/悬空引用/删除依赖有诊断 |
| CTL-02 | P1 | 可选 Admin API 与 v1 resources 适配 | CTL-01,EXT-04 | services/routes/target-groups/targets、If-Match、token 权限、监听边界、审计；无旁路写状态 |
| CTL-03 | P1 | runtime 持久化与恢复协议 | CTL-02 | crash injection 覆盖写盘/提交所有窗口；幂等重放、损坏恢复、并发重启，不丢已承诺写入 |
| CTL-04 | P1 | warming/active/draining/failed 生命周期 | CTL-02,RT-05 | 活动连接计数、超时、旧 H2 stream 与 target 移除行为；部署切流场景通过 |
| OBS-01 | P1 | 可选 OTLP traces / metrics | EXT-04,RT-04 | trace 信任、采样、基数、导出队列有界；body 完成与取消观测准确 |
| OBS-02 | P1 | 观测与运维隔离策略 | EXT-02,CTL-02 | 保留普通 metrics/health 路由；可选管理 listener；业务认证故障不误判 liveness |
| OPS-01 | P1 | v1 CLI、adapt/fmt、passwd/precompress 工具恢复 | BASE-01,EXT-05,AUTH-01 | 命令/参数/退出码台账；旧 examples 自动转换并验证，有损转换报告 |
| OPS-02 | P1 | Windows Service / systemd / notify / socket activation | NET-03,RT-01 | 安装卸载、信号、权限、shutdown 实测；不在测试机默认修改系统服务 |
| OPS-03 | P1 | deb/rpm、容器、安装脚本、签名发行 | QA-01,OPS-02 | 包安装/升级/卸载隔离测试；产物、SBOM、校验与签名有实际执行证据 |
| QA-01 | P0 | 执行现有 Windows/Linux/macOS CI 与供应链检查 | 无 | 配置存在不算通过；记录平台/commit/命令；修复真正失败而非放宽门禁 |
| QA-02 | P0 | testkit/conformance、旧版差分场景与效果组合矩阵 | BASE-01,EXT-02 | 新老二进制隔离端口，同请求对照状态/头/body；有意变更单列 |
| QA-03 | P0 | 浏览器、curl、真实 H2/H3/PHP 等互操作 | QA-02 | 按模块落地增量执行；真实浏览器验证 CORS，真实 H2 GOAWAY/多路复用 |
| QA-04 | P0 | soak/fuzz/性能门禁持续执行 | BASE-02,RT-02 | 每批新增协议加入故障注入；RC 跑长稳和真实负载，FD/task/pool 不持续增长 |
| REL-01 | P1 | 完整化候选与 v1 迁移发布 | 所有 P0/P1 | 旧台账无未解释缺失；默认/最小/每可选包测试；限制、迁移、回退、产物齐全 |

QA 任务持续执行，不能理解为等所有功能写完才开始测试。任何需要外部域名、CI 推送权限或发布权限的验收单独列出条件，其他工作可继续。

## 5. 实施顺序与交付门槛

| 批次 | 内容 | 退出条件 |
|---|---|---|
| A 基线与真实验证 | BASE、QA-01、RT-01～04 | 已知行为有台账，基础事务/协议及平台问题清楚 |
| B 扩展契约 | EXT-01～04、RT-05、QA-02 | 一个外部插件完整接入并真实裁剪；高风险组合可解释、可拒绝 |
| C 第一批能力恢复 | CGI-01～02、WEB-01～04、AUTH-01、TLS-01 | PHP-FPM 可用；标准 Web 与认证差距缩小；每项配套互操作 |
| D 控制面与证书 | CTL-01～04、TLS-02～04、OBS | Admin 持久化/并发/恢复通过；旧 TLS 主要能力补齐 |
| E 协议与内容完整化 | CGI-03～04、NET、WEB-05～06、RT-06 | TCP/UDP/H3/CGI 等各自通过真实后端和取消/drain 验证 |
| F 工具与完整化发布 | EXT-05、OPS、REL-01、QA-03～04 | 每项 v1 功能有最终归属，发布与迁移可实际执行 |

无依赖的工作可以并行，但不要并行修改同一条事务正确性链。具体人力/日历估算在 A、B 完成后用实际吞吐和剩余风险重新估算，不承诺固定人周。

可先发布范围明确的 2.0 子集，但必须公开剩余台账；只有 REL-01 通过才能宣称“旧 main 功能完整化”。可选 feature 本身就是完整迁移的一种交付方式，不要求全部默认开启。

## 6. 验收与状态维护

- 每个任务落地同时提交功能、测试、文档、feature manifest 与兼容台账更新，避免只登记 descriptor 而无运行实现。
- 发布构建至少覆盖默认、最小、各可选包、全功能可行组合；认证/缓存/压缩/body-limit/retry 采用高风险组合矩阵，不只测孤立指令。
- 性能在同机同负载重复测量；暂以吞吐回退 5%、P99 上升 10%、稳态 RSS 上升 15% 作为调查阈值，不视作未经测量的硬 SLA。超阈值解释原因、记录收益和接受决定。
- prepare 失败、stale、NoOp 不应改变活动行为；成功提交后外部 DNS/CA 等故障允许 degraded，但必须有恢复和 readiness 契约。
- reload 稳定性至少覆盖 H1 keep-alive、H2 后续 stream、WebSocket、大 body、CGI 子进程、TCP 半关闭、UDP 会话与 H3。超时后的强制关闭要符合文档，而非笼统承诺永不丢请求。
- 不恢复旧版的不安全默认值，不直接从测试数或代码量推导生产成熟度。

Kubernetes、Shared State、WASM、AI Gateway、OCSP 和零停机二进制升级作为独立后续候选，不阻塞恢复旧 main 能力。也不为这些尚未实施的功能提前引入万能 API。
