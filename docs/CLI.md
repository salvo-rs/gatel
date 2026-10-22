# CLI 与生成的使用文档

命令结构以 `crates/gatel/src/main.rs` 的 Clap 定义为唯一来源。`--help`、Shell 补全和 roff 手册使用同一命令树；它们描述当前 v2，不包含尚未恢复的旧命令。帮助/补全/手册生成不读取配置、不绑定端口、不修改信任库，也不自动安装生成物。`serve` 则会实际启动 HTTP 监听，见下文。

## 热更新时排空旧连接

```kdl
global {
    http "127.0.0.1:8080"
    reload-drain "30s"
}
```

默认省略：已接受的连接固定原 Generation，后续 H2 stream 和 H1 keep-alive 请求仍使用旧配置。
显式启用后，每次成功新代激活在 Server 同步时通知旧代连接停止接收新请求；H1 关闭 keep-alive，
H2 通过 Hyper 发起优雅关闭（GOAWAY）。在途响应可在指定期限内完成，期限从通知开始计算，
到达时强制释放仍在运行的请求、TLS/PROXY 握手及升级隧道，并归还连接额度。
新连接使用新代。NoOp 和被拒绝/准备失败的更新不发起退役；显式资源 refresh 发布新代会发起。

时长允许 0（立即关闭）至 365 天。后续更新不延长或取消已宣布的期限，删除此项只影响尚未退役的连接。
这是一项可中断长请求/WebSocket 的部署策略，不是逐用户撤销，不保证撤回上游已经执行的操作，
也不等同于停止/重启不可中断的插件计算。现有 `grace-period` 继续控制监听器移除和进程关闭。
`explain`/`diff` 显示该策略；真实连接回归见 `gatel-http/tests/reload_drain.rs`。

## 可选管理监听

带 `admin-api` feature 的构建可在 `run` 中成对提供 `--admin-listen` 和
`--admin-token-file`，显式启用 loopback 配置管理（默认内存模式，可选下面的检查点模式）。默认关闭，不接受命令行明文 token；
权限、请求契约、退出顺序及未持久化边界见 [ADMIN.md](ADMIN.md)。

可额外提供 `--admin-read-token-file PATH`，为监控提供只能 GET 版本/状态的独立凭据。
读写 token 都来自私有文件且必须不同；此参数不替代必需的写入 token 文件，也不会启用动态轮换。

## 检查点引导与恢复诊断

### 可写检查点模式

```sh
gatel run gatel.kdl --checkpoint-journal /private/gatel-journal --checkpoint-instance deployment-id
```

仅带 `admin-api` feature 的构建提供，两参数须成对使用，与 `--bootstrap-journal` 互斥。
目录必须已存在并由操作者限制权限（Unix 私有权限、Windows 私有 ACL）；检查点含全部来源
配置，可能包含敏感内容。新目录从文件启动并保存；已有有效检查点优先恢复服务与最新观察
双快照，随后文件 watcher 与可选 Admin 更新共用同一检查点会话。默认不开放 Admin 端口；
如需管理接口，仍须另外提供监听和 token 文件参数。

这是 **checkpoint-only 模式，不是 WAL 或幂等回执协议**：更新经激活并保存后才返回成功；
崩溃若发生在保存前，重启可能回到旧检查点。HTTP 503 可能表示已生效但保存结果不明，
不能盲重试。watcher 遇到非 CAS 提交错误会触发宿主排空并以 3 退出，不自动退回内存模式。
实例不匹配、损坏、未覆盖日志或锁冲突均拒绝启动，不自动修复。

**该模式的 `--dry-run` 会初始化/保存检查点并实际绑定资源，再关闭退出**，不是只读检查；
已有检查点会恢复并验证绑定，不覆盖为当前文件输入。只读诊断请使用 `journal-inspect`。
启动发出 `checkpoint_mode` 警告。提供下面的离线压缩导出；在线轮转、幂等查询、完整崩溃/断电验收仍待完成。

### 离线压缩导出

```sh
gatel journal-compact --directory /private/gatel-old --instance deployment-id --destination /private/gatel-next
gatel journal-inspect --directory /private/gatel-next --instance deployment-id
```

仅 `admin-api` 构建提供。先停止原目录的写者；命令以现有 writer.lock 独占锁读取原目录，
不修改或删除原日志/检查点。目标必须不存在，父目录必须受信任且已存在，Unix 要求私有权限，
Windows ACL 由操作者限制。父目录的目标创建/改名需由操作者串行管理，不是抵御恶意本地用户的沙箱。

要求检查点恰好覆盖完整日志：缺少检查点、未决事务、不完整尾部、损坏、不兼容构建或检查点后的
任何新意图（包括已拒绝意图）都会拒绝。完整覆盖的检查点内仍可保留被拒绝的 observed 快照。
命令保留两份来源及原发布身份，将检查点重绑定到仅包含实例绑定记录的新日志；序号从 1 重启，
旧事务历史不进入目标，不能把旧序号当作跨目录的幂等回执或查询身份。

三份文件先在同一父目录的私有 `.gatel-compact-*` 暂存目录中写入并同步，再整体改名发布。
Unix 另同步目录。失败后需检查目标：改名之后发生错误时，目标可能已经完整存在；重复命令不覆盖目标。
强杀可能留下含敏感配置的暂存目录，不要用它启动；原目录保留，命令不自动清理未知残留。
成功后仍需人工检查并将启动参数切换到新目录；不会自动切换运行实例，也不删除旧目录。
这不是在线/原地轮转、WAL 集成或断电保证；不要同时以同一部署身份运行新旧目录。

Windows/Linux 真实子进程强杀回归覆盖日志同步后、检查点同步后、发布前和发布后四个阶段，
验证原文件不变、原写者锁释放、目标只在整体发布后存在、恢复保留两份来源，以及重试不改动
遗留暂存文件。测试复用实际磁盘发布代码，在测试回调处暂停后由父进程终止；Linux 验证 SIGKILL。
这比错误返回注入更接近进程崩溃，但不覆盖系统掉电、磁盘控制器缓存或任意指令处的全部窗口。

Windows/Linux 长驻子进程回归已验证真实 HTTP 就绪、并发 CLI 被写锁拒绝、检查点保存
故障使 watcher 驱动进程排空并以 3 退出，以及监听端口/写锁释放。修复测试路径后可恢复
最后确认的检查点；该证据不等同于操作系统信号、强杀或断电恢复验收。

另有 Windows/Linux 真实 Admin 跨进程测试：PUT 收到 200 后强制结束该测试进程，
损坏当前文件再启动仍能通过 HTTP 访问已确认的 Admin 路由；旧进程 ETag 返回 412。
这证明该测试中的已确认更新经进程终止后可恢复，不代表保存中途所有崩溃窗口或机器断电保证。

同组测试还覆盖成功更新之后的语义拒绝（422）：离线检查确认服务/观察快照不同，
重启继续服务先前有效路由，新 ETag 可用于修正被拒绝的 Admin 输入。非 200 不等同于
来源意图完全没变化，客户端仍须重新读取版本。

### 从检查点只读引导服务

带 `admin-api` feature 的 `run` 可显式使用：

```sh
gatel run gatel.kdl --bootstrap-journal /private/gatel-journal --bootstrap-instance deployment-id
```

这是已有检查点的只读引导，**不是运行期持久化模式**。后续文件监听和 Admin 更新仍只在内存
应用，不写回该日志/检查点；重复使用该选项重启会再次使用旧检查点。不要把此模式用于要求更新
跨重启保存的生产部署。启动会输出 `checkpoint_bootstrap` 警告；`--dry-run` 可验证资源绑定并退出。

只接受完整、兼容、正确实例的 GATELC02 检查点且覆盖全部日志；缺失、短尾、未知结果以及检查点
之后的任何记录（包括拒绝）都退出 3，不自动修复或退回磁盘配置。校验发生在创建管理监听前。
已有日志独占锁保留到服务关闭，文件不修改。该命令不能创建初始检查点。

引导先使用检查点服务来源，不读取根 KDL；仍按命令指定的根目录解析外部资源。额外文件来源必须
继续通过 `--source NAME=PATH` 声明，未授权来源会拒绝。启动后的 watcher 才观察当前磁盘配置，
按恢复的修订接续，并保留其他来源的最新意图；磁盘后来变化可能重新配置服务。

### 检查日志与检查点

带 `admin-api` feature 的构建提供：

```sh
gatel journal-inspect --directory /private/gatel-journal --instance deployment-id
```

仅检查已有的 `writer.lock`、`intent.log` 与可选 `checkpoint.bin`。需停止持有该日志锁的
写入者，并使用创建日志时的稳定实例标识；标识不是 token。命令获取已有独占锁，但不创建
目录或文件、不追加/修复日志、不保存检查点、不清理临时文件、不准备运行时或绑定监听。
日志以只读句柄打开；Windows 的已有锁文件仍需读写权限。读取可能更新文件系统访问时间。
检查点须与本构建的插件注册表、计划和日志前缀一致；校验失败不会忽略或回退。

输出为 `key=value` 行，仅含静态状态、日志/检查点/意图序号、最终决定与短尾标志；
不输出配置正文、来源树、实例标识或文件路径。错误使用统一脱敏信息。

双快照摘要字段 `checkpoint_serving_sources`、`checkpoint_observed_sources` 和
`checkpoint_empty_observed_sources` 分别表示已发布来源数、最新观察来源数及其中空贡献数；
`checkpoint_observed_differs` 表示两份已校验快照的修订向量是否不同。无检查点时以上字段为
`none`，不能当作 0 或 false。差异可能来自 NoOp、空贡献或拒绝更新，不代表观察配置一定非法；
也不包含检查点之后的日志意图。摘要不输出来源名称或内容指纹，不改变原有退出码含义。

- 退出 `0`：检查点覆盖日志，或其后仅有已拒绝意图；不代表实际服务已恢复或健康。
- 退出 `1`：仅有初始化绑定，或存在短尾、未知结果、缺失检查点、检查点后成功应用，需要协调。
- 退出 `2`：命令参数用法错误（Clap 帮助/错误可回显参数，不要传入秘密）。
- 退出 `3`：存储缺失、锁冲突、权限/身份/完整性/构建校验失败，或输出失败。

这是只读诊断入口，不执行上述可写模式的启动或请求提交；诊断成功不是服务就绪或写入回执。

## 静态资源预压缩

静态资源可通过 `gatel precompress ROOT` 生成 gzip/br/zstd 文件，支持 dry-run、扩展名
筛选和原子 sidecar 替换。已有同名普通文件会被覆盖；离线目录、权限和失败边界见
[PRECOMPRESS.md](PRECOMPRESS.md)。

## ACME 缓存目录与升级

certon 0.3 的证书持久化使用 `CertStore` 接口，新写入为同一账户命名空间内的
`certificate-<hash>.identity`，同时保存证书、私钥和 issuer 元数据。
旧 `<host>.identity` 与 `.crt`/`.key` 在新记录缺失时仍可读取，不要求立即重新签发。
新记录损坏不回退旧记录；删除标记阻止旧缓存复活。详见 [CERTON.md](CERTON.md)。

签发前会先保存 `registration-intent.key`，注册响应丢失后重试沿用同一账户私钥。
该文件不是临时文件，应与账户记录一起备份；不要在运行时删除或替换。
它与已提交账户密钥冲突或内容损坏时会停止本次签发并报告错误。
根据 [RFC 8555 §7.3.1](https://www.rfc-editor.org/rfc/rfc8555.html#section-7.3.1)，
同一账户密钥可以重新取得已有账户 URL；这不等于保证任意文件系统的断电持久性。

账户注册信息与私钥现在保存在同一 `account-<hash>.key` 原子记录中。
旧的独立账户 JSON/KEY 文件保留，但不会自动导入；已有证书身份缓存仍可复用，
下一次需要签发时可能注册新账户。升级前应备份账户目录并评估 CA 注册限额。
损坏的新账户记录明确报错，不会静默回退到旧文件或反复创建新账户。

同一账户命名空间中的签发现在使用 `account.lock` 操作系统文件锁协调。
锁文件会长期保留，不能在进程运行时删除；等待锁可以取消，进程退出会释放锁。
等待后会先复用其他进程写入的未进入续期窗口的证书，避免重复申请。
只适用于遵守该协议且文件系统支持锁的进程；不要与旧版写入者混用同一目录。
这不提供 NFS/分布式一致性，也不负责将 HTTP-01 验证请求路由到持有挑战的副本。

配置中的 ACME `directory` 是存储基础目录，实际账户数据和证书身份文件位于
`accounts-v1/<CA URL 与邮箱的稳定哈希>/` 子目录。更换 CA 或邮箱会使用不同缓存；
仅调整续期窗口不会更换目录。备份应包含整个账户命名空间，其中含私钥。

旧布局直接放在基础目录的账户文件、`.crt`/`.key` 或 `.identity` 不会被删除，
也不会自动导入，因为其账户来源不明确。升级可能需要重新签发，请先确认 CA
可用性和签发限额；本版本尚无自动迁移命令。Unix 账户目录必须限制为 0700，
Windows 使用继承 ACL，需要另行检查目录访问权限。

## 多个命名文件来源

```sh
gatel validate gatel.kdl --source backends=backends.kdl --source routes=routes.kdl
gatel explain gatel.kdl --source backends=backends.kdl
gatel run gatel.kdl --source backends=backends.kdl --source routes=routes.kdl
```

主配置来源固定为 `file:main`；附加来源均为 `file:NAME`，不是 Admin 或 Kubernetes
身份。来源集合在启动时固定，最多 31 个附加来源；名称为 1～64 个 ASCII 字母、
数字、`-`、`_`，不得使用 `main` 或重复名称。格式/名称错误退出 2，重复名称或
来源数量超限在宿主构建时退出 3；文件/配置错误沿用退出 1。

命令行路径相对当前工作目录；每个文件内部 import 相对该文件，业务资源路径
（静态根、证书、Secret 等）仍统一相对主配置目录，不改为各来源目录。
资源按名称联合，重复声明或悬空引用明确拒绝；整个集合最多一个 global。
相同文件被作为不同来源重复传入也不会按路径静默去重，可能产生资源冲突。

各来源分别保留观察版本，任一文件读取/语法失败不更新半套观察缓存；后续统一走
编译、prepare、CAS 发布，失败继续服务 LKG。读取多个文件本身不是文件系统原子
快照：部署者仍须协调跨文件修改，或用原子替换的单个配置树发布一组变更。
启动时非递归订阅所有已发现配置文件的父目录；每次重载尝试后重新协调目录集合，
先添加新目录，再移除不再需要的订阅，操作失败后续重试。新 import 目录、丢失目录
和文件系统通知遗漏仍由五秒轮询兜底。import 必须为相对路径且不能含 `..`；
任意外部配置应通过启动参数 `--source` 注册，而非绝对路径 import。
运行测试覆盖实际 watcher 循环到 HTTP 新响应，不承诺由文件事件
而非兜底轮询触发，也不保证各平台事件交付时序。`fmt` 暂不接受 `--source`。
监听器不可用或通知通道关闭时仍等待五秒轮询期限，不将关闭通道视作持续到达的
配置事件。该间隔是等待时间；还需加上消抖和一次配置处理耗时，并非固定频率 SLA。
主重载、配置文件发现及父目录路径规范化共用每宿主一个名额的后台阻塞任务入口；
排队等待不占用 blocking 线程，关闭后拒绝新扫描并等待在途工作结束。
操作系统 watch/unwatch 调用仍在监听协调任务执行。
普通修改通知只匹配已发现配置文件的文件名（不是扩展名），减少同目录日志/静态
文件写入导致的无关重载；同名文件会保守触发。创建、删除、重命名、无路径修改及
需要 rescan 的通知保留，配置发现完成前不过滤修改。这个过滤器不在通知线程做
文件 I/O。发现失败后暂时缺失的 import、外部证书/Secret 和路径拼写差异仍由轮询兜底。
真实 CLI 集成测试覆盖附加来源拒绝后旧响应可用、修复后发布新响应；Linux 还覆盖
SIGTERM 后正常退出、shutdown_complete 事件与端口不再接受连接。该场景在恢复发布
完成后发送信号，不等于覆盖任意 prepare 阶段的 OS 信号时序；Windows Ctrl+C 仍待验收。

比较两套完整来源集合：

```sh
gatel diff old/gatel.kdl new/gatel.kdl --before-source backends=old/backends.kdl --after-source backends=new/backends.kdl
```

`--before-source` / `--after-source` 可分别重复，各自独立遵循上述名称、数量、
路径与联合校验规则，不自动从另一侧继承来源。来源身份应保持稳定；改名可能改变
资源所有权。命令只编译并预览状态复用决策，不绑定监听、不执行运行时 prepare，
不能代替上线前的证书、目录、端口可用性检查。任意一侧配置失败退出 1；
有效比较无论有无差异均退出 0。此接口也可比较不同来源集合，但不意味着运行中
的宿主已支持在线注册或删除来源。

差异输出的 `resources` 段单独标明资源增删、来源所有权迁移及规范化配置变化，
包括中间件、响应内容等未引起状态重建的修改。配置变化摘要不打印参数值或其
哈希；它是资源级提示，不是完整字段级 diff。`state: all reused` 只表示状态槽可
复用，不表示请求行为未变；只有 `no change` 表示编译计划一致。

## 配置格式化

```sh
gatel fmt gatel.kdl
gatel fmt gatel.kdl --check
```

`fmt [CONFIG]` 默认读取 `gatel.kdl`，将规范化 KDL 输出到 stdout，保留注释和
字符串内容。`--check` 不输出正文：格式已一致退出 0，需要格式化退出 1。
语法错误、读取失败、非 UTF-8 或超限也退出 1；参数错误退出 2，输出 I/O 失败
退出 3（关闭的输出管道视为正常结束）。

格式化属于 KDL 前端，不编译业务配置、不加载插件、不展开 import、不绑定端口，
也不写入任何配置文件。缺失 import 或当前构建不支持的插件不阻止语法格式化；
业务正确性仍须执行 `validate`。每次仅处理一个普通文件，沿用 4 MiB 文件上限
和前端嵌套深度限制。请勿将 stdout 直接重定向到输入文件，shell 可能先截断它；
需要保存时先输出到另一个文件，检查后再替换。不支持 stdin、递归格式化或原地写入，
也不等同于尚未实现的 v1 `adapt` 迁移器。

为保留 `/-` 禁用属性与行内注释，格式化会保留参数/属性的原始格式及含注释的
节点间隙；主要规范化节点缩进与终止符，不承诺统一所有行内空格和数字写法。

配置加载与格式化共同限制块注释复杂度：最多 16 层嵌套，单个块注释最多
64 个解析片段（嵌套起止、单独的 `/` 或 `*`、连续普通文本各计一步）。
这是对当前递归 KDL 解析器的保护，不是 KDL 语法本身的限制；较复杂的注释
应改为多行 `//` 注释。长的普通注释文本不按字符数消耗该片段预算，仍受文件大小限制。

连续 `/-` 禁用前缀也共享 16 层节点深度预算；空白、注释或续行不会重置连续
前缀计数。已经给出值的独立禁用项不累计成递归深度，字符串中的 `/-` 不计数。

## 快速静态目录服务

```sh
gatel serve
gatel serve ./public --port 8081
gatel serve ./public --browse
gatel serve ./public --listen 0.0.0.0 --port 8081
gatel serve ./public --listen ::1 --port 8081 --dry-run
```

默认 ROOT 为当前目录、port 为 8080、listen 为 127.0.0.1，目录浏览关闭。`-p/-l/-b` 分别对应 port/listen/browse。为避免开发目录无意对外开放，默认监听地址不恢复旧版的 0.0.0.0；显式监听所有地址前确认目录内容可以公开。此命令没有认证或 TLS，外部部署应使用显式配置和访问策略。

监听参数接受 IPv4/IPv6 地址，不做 DNS 解析；端口为 1～65535。`--dry-run` 会验证目录并实际绑定端口，然后走正常关闭流程退出，端口被占用时返回 3。根目录不存在或不是目录也返回 3；参数格式错误返回 2。

该命令构造内存中的类型化 Snapshot，复用标准 normalize/compile/prepare/commit、file-server 与 App 关闭流程，不生成临时 KDL，不读取或监控目录里的 gatel.kdl。路径保留为 PathBuf，不拼接进配置文本。静态内容变化由后续请求直接读取；CLI 选项不热更新，需重启进程。Ctrl+C（Unix 也支持 SIGTERM）发起正常关闭。

默认 index.html、隐藏文件/路径约束、Range、HEAD 和目录浏览规则沿用 v2 file-server；没有另建简化 HTTP 实现。Windows 测试覆盖真实请求及 dry-run；Unix 测试额外覆盖 SIGTERM、含引号/换行和非 UTF-8 目录路径。Windows 真实控制台 Ctrl+C 与所有平台组合仍需独立验收。

## 上游 TLS 凭据轮换

使用配置文件运行时，现有文件事件监听及约 5 秒的兜底轮询也检查上游 TLS 的
CA、客户端证书和私钥。内容变化会触发资源刷新，无需修改 KDL；内容未变仍为
NoOp。监听目录外的凭据依赖轮询发现。损坏、缺失或证书/私钥不匹配时刷新失败，
继续使用旧代；这不是紧急撤销，旧连接可能继续完成。请使用受控的文件替换流程，
当前不保证多个凭据文件的原子快照。此能力不泛化到所有插件秘密或服务端证书。

## 密码哈希生成

```sh
gatel passwd --stdin < password.txt > user.hash
```

Unix Shell 中先设置 `umask 077`，确保新建输入/哈希文件仅当前用户可访问；已有文件不会因 umask 自动收紧，需自行核对权限。当前 Secret 加载器拒绝组/其他用户可访问的 Unix 文件（通常使用 0600）。Windows 请设置合适的 ACL，本工具不会替你修改权限。

该命令默认输出一行带随机盐的 bcrypt 哈希，用于 `basic-auth { user "alice" hash="file:user.hash"; }`。以 `--features argon2` 构建后，可用 `gatel passwd --stdin --algorithm argon2id` 生成 Argon2id PHC；未编译时该选项不可用。它不修改现有配置或 Secret 文件；重定向由 Shell 执行，注意避免覆盖需保留的文件，并限制输入文件及哈希文件的访问权限。

- 必须显式传 `--stdin`，输入来自管道或文件并以 EOF 结束。若 stdin 是终端则拒绝，不提供会回显明文的交互提示；隐藏输入模式仍待实现。
- 不接受明文密码位置参数。不同于旧 gatel-passwd，不把密码放进命令历史或进程参数；避免使用包含真实密码字面量的 Shell 命令来构造管道。
- 保留首尾空格，只移除末尾一个 LF 或 CRLF；拒绝空密码、剩余换行/回车、NUL、非 UTF-8。bcrypt 0.17 的 non_truncating_hash 为终止字节保留空间，因此本工具接受 **1～71 UTF-8 字节**，不是字符数；不静默截断。
- Argon2id 密码允许 1～256 UTF-8 字节，拒绝规则与空白保留规则相同。根据算法最多读入 74 或 259 字节（上限、可选 CRLF、溢出标记），不缓存无限输入。输入缓冲使用 Zeroizing，启用算法库提供的 zeroize；这不保证操作系统、Shell 或上游程序中的全部副本被清除。
- bcrypt 的 `-c/--cost` 默认 12，允许 4～16；Argon2id 拒绝此选项并采用固定生成策略（m=19456 KiB、t=2、p=1）。认证请求侧参数与预算见 [AUTH.md](AUTH.md)，其他算法与可配置宿主预算仍未完成。
- 参数错误退出 2；输入、随机数/哈希或输出错误退出 3，诊断不包含 stdin 密码。只有成功结果写 stdout；不要将错误输出当作哈希使用。

回归见 `gatel/tests/passwd.rs`：真实子进程生成、随机盐、输入拒绝、错误码，以及生成哈希 → Secret 文件 → 实际 HTTP Basic Auth 的正确/错误密码验证。旧独立 gatel-passwd 可执行入口尚未提供；本轮恢复的是整合后的 `gatel passwd`。

## Shell 补全

```sh
gatel completions bash
gatel completions elvish
gatel completions fish
gatel completions powershell
gatel completions zsh
```

生成内容写到 stdout，按对应 Shell 的安装方式自行保存和加载。例如在 Bash 当前会话中使用 `source <(gatel completions bash)`。这些是静态命令/选项补全，不读取运行中配置或查询 Admin API。升级命令行后应重新生成。

## 手册

```sh
gatel man-page > gatel.1
gatel man-page run > gatel-run.1
gatel man-page completions > gatel-completions.1
```

根手册包含子命令索引，每个子命令可单独生成，支持按空格分隔的子命令路径。输出为 roff，不是终端渲染后的纯文本；在有 man 工具的系统上可用 `man ./gatel-run.1` 查看。未知路径返回退出码 2，并且 stdout 为空。输出没有生成时间戳，同一构建及参数重复生成内容一致。

## 退出码与输出

| 退出码 | 含义 |
|---|---|
| 0 | 成功；生成文档时下游提前关闭管道也正常结束 |
| 1 | 配置被拒绝 |
| 2 | 命令用法错误，包括不支持的 Shell 和不存在的手册路径 |
| 3 | 运行失败，包括生成文档时非 BrokenPipe 的输出错误 |

正常的补全/手册内容只写 stdout，错误写 stderr。生成器先写内存，再统一处理实际输出及 flush 错误。该行为不改变 run/validate/explain 等现有命令的输出约定。

验证见 `crates/gatel/tests/cli_artifacts.rs` 和 `src/cli_output.rs`：五种 Shell、所有当前业务子命令、当前选项/默认配置路径、确定性、无多余日志、非法调用、write/flush 失败。生成测试不等同于每种 Shell 的交互安装验收，也不等同于所有平台的 man 渲染验收。

2026-09-07：Windows 默认/全 feature 宿主测试和 Linux 全工作区测试通过；生成的 PowerShell 补全通过宿主 AST parser，Bash 补全通过 Linux `bash -n`，尚未逐项验收交互候选行为。
