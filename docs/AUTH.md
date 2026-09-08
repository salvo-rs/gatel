# Basic Auth：工作预算与失败边界

预算的当前占用、限额、拒绝原因与工作结束计数见 [METRICS.md](METRICS.md)。

## 配置准备

哈希仍由 SecretRef 在 prepare 阶段加载，保留文件权限检查与错误脱敏。默认接受规范的 60 字节 `$2a$`/`$2b$`/`$2y$` bcrypt，检查分隔符、两位数字成本、bcrypt Base64 字符和 salt/digest 的未用位，成本仅允许 4～16。可选 Argon2id 见下节；配置检查不执行密码计算。

每个指令最多 1024 个用户；用户名为 1～256 UTF-8 字节，不含冒号、CR、LF、NUL。错误或算法参数超预算哈希在 prepare 拒绝，保留 LKG，不到第一次登录时才报错。配置指纹使用 `gatel-basic-auth-v4` 行为域，并包含已编译算法名称，不把哈希材料写进日志或公开指纹。配置中直接写入 `$...` 编码哈希会拒绝，避免将它误当文件路径后泄漏进错误信息。

## 可选 Argon2id 与共享策略包

`gatel-password` 不依赖 HTTP、配置或 Tokio，集中实现参数验证、不可变 Credential、验证和生成；Credential 的 Debug 脱敏，编码材料使用 Zeroizing 持有。HTTP 准备阶段构造 Credential，阻塞计算阶段只调用其 verify；CLI 使用同包生成策略。该包尚非第三方动态算法 ABI，不允许插件绕过宿主工作预算。

`cargo build -p gatel --features argon2` 同时开启认证与 CLI 的 Argon2id。默认构建不链接 argon2/password-hash/PHC 算法依赖，不声明 Argon2id 能力；未编译时此类 Secret 在 prepare 拒绝。`gatel capabilities` 显示实际密码算法，CLI 的 algorithm 候选也按 feature 裁剪。

Argon2 0.6 的 PHC 输入最大 256 字节，仅接受 argon2id、版本 19、明确的 m/t/p 三个参数、16 字节盐和 32 字节摘要，不接受 keyid/data、缺少参数、其他变体或版本。m 范围 8192～32768 KiB、t 范围 1～4、p 范围 1～4，先解析/限制参数再计算；并未声称范围内每个参数组合都适合每个部署的密码策略。

生成策略固定 m=19456 KiB、t=2、p=1、输出 32 字节并使用随机盐，不允许 CLI 误把 bcrypt cost 用在 Argon2id 上。Credential 在纯解析时记录声明工作内存：Argon2id 按已验证的 m × 1024 字节，bcrypt 预留 8 KiB。认证按实际选择的凭据申请任务与内存额度；未知用户名也按替代凭据申请。默认宿主共享 16 项任务与 256 MiB 声明工作内存，任一不足均拒绝准入。线程栈、分配器、凭据和运行时开销不在此数值内；这不是进程 RSS 上限。未开启 Rayon parallel feature。

## CPU 工作归属

请求验证通过 `gatel-plugin-runtime::blocking` 的 Executor 执行，内部使用 Tokio spawn_blocking，不在异步请求线程上运行密码计算。Server 在 Resources → BuildContext 中注入同一执行句柄，标准认证不再自行创建池。CLI 的单个 App 拥有一份可配置预算，覆盖其路由、站点与新旧代；改名、重载或重新 prepare 不会获得另一套额度。嵌入式 `Server::new` 仍使用固定的进程级默认池；多个显式创建控制器的 App/宿主各有独立预算，不再宣称这些独立控制器合计受 16 项/256 MiB 限制。需要跨宿主合并计量时由嵌入式宿主显式共享同一控制器，且统一配置写入权，不能让独立配置源争用同一个限额。

任务数与声明内存必须在同一锁下原子预留后才提交工作，不建立准入等待队列；Tokio 阻塞队列里的工作也计入额度。额度不足、预算关闭均返回 503，不发送 WWW-Authenticate；错误凭据或无效头返回 401，仍附带认证 challenge。任务 panic/运行时取消等执行失败也返回 503，不允许认证成功。

嵌入式宿主可持有 Budget 控制器，通过 `Server::with_blocking_executor` 注入 Executor，使用 reconfigure 原子调限；允许 1～1024 项任务和非零 u64 字节内存上限。调低至在途占用以下不取消工作、不清零使用量，直到释放足够额度才准入。非法调限不改变状态；close 永久关闭新准入，后续 reconfigure 或配置提交不重新开放。插件拿到的 Executor 没有调限权限，但源码链接插件仍是受信任代码，不是防止恶意插件另起线程的沙箱。Admin API 与按租户公平调度尚未接入；宿主不得在候选 prepare 阶段修改共享预算。

### KDL 配置与提交

```kdl
global {
    http "127.0.0.1:8080"
    blocking jobs=8 memory="128MiB"
}
```

每个 global 最多一项 blocking；jobs 默认 16、memory 默认 256 MiB。非法值、重复节点和未知属性在规范化拒绝，数值参与配置指纹。删除 blocking 会在下一次成功提交恢复默认值，不清零使用量；旧连接也立即遵守新准入额度，但仍使用原代凭据，因此这不是凭据撤销。

CLI App 自动提供控制器。嵌入式宿主需同时提供 `Resources.blocking_budget` 与匹配的 Executor；所有者不匹配或显式 blocking 配置缺少控制器时 prepare 拒绝，不静默忽略。准备过程只构造已验证、未生效的 PreparedChange，成功候选在版本复核后才于提交锁内取得预算锁，发布新代后释放。失败/stale/NoOp 候选不调限；丢弃未发布 guard 恢复原限额。控制器手工 reconfigure 是嵌入式宿主操作，不写回配置；NoOp 不覆盖这种外部操作，下一次真实提交重新应用配置。

两种额度一起移动到阻塞闭包中：即使请求超时、连接断开、上层 future 被取消，仍保留任务与内存占用，直到闭包真正返回或展开 panic。不能把丢弃 JoinHandle 当作停止计算；close 也不取消已准入工作。Tokio 正常销毁运行时会等待已启动的阻塞任务，因此不承诺认证工作在 HTTP grace 到期时立即停止，也没有硬墙钟计算期限。有限成本和并发限制是资源边界，不是实时调度保证。

有效格式的未知用户名也执行一次验证（使用当前用户表首个哈希作为替代）；已知用户名只验证对应哈希，不对全部用户逐个 bcrypt。不同用户的成本可以不同，查找与比较也不承诺恒定时长，因此不能宣称完整的用户名计时隐藏。需要更强的隐私或滥用控制时，仍需统一成本、边缘限流及独立策略；后续规划包含这些能力。

## 输入和迁移差异

- 拒绝重复 Authorization 头；Basic scheme 大小写不敏感。
- 编码凭据最多 1024 字节，标准 Base64 必须正确补齐，拒绝 padding 后附加数据和非零未用位；不继续接受宽松解析产生的别名。
- 用户名最多 256 字节，密码总体最多 256 字节，内容必须 UTF-8、不含 CR/LF/NUL。保留首尾空格和密码内冒号。不改变已存在的空密码哈希是否匹配，但新 passwd 工具拒绝生成空密码。
- bcrypt 凭据验证仍拒绝超过 72 字节的密码，不截断有效前缀；已有合法 72 字节密码可验证，CLI non_truncating 接口按其终止字节规则限制新 bcrypt 密码为 71 字节。Argon2id 生成/验证上限均为 256 字节，详见 CLI.md。
- 旧连接仍固定接受时的 Generation。更新凭据不会追溯撤销已有连接使用的旧配置；紧急撤销/drain 是 RT-05 的后续任务，不能用本轮 CPU 额度复用代替该语义。

## 验证

`gatel-plugin-runtime/src/blocking_tests.rs` 验证运行中/阻塞队列内取消均保留两种额度、panic 回收、超额不执行、溢出拒绝、非法调限不变、缩容保留占用、关闭不可重开以及多线程并发准入。`basic_auth.rs` 测试验证忙时 503、密码长度、重复头和规范 Base64；`gatel-password` 测试验证 bcrypt/Argon2id 生成验证、声明内存与参数对应、未编译算法拒绝与 Debug 脱敏。

`gatel-http/tests/auth_budget.rs` 注入自定义宿主预算，在真实 HTTP 上验证有任务槽但无内存时拒绝、成功 reload 与失败 prepare 保留在途占用、调低额度不清零、恢复额度后认证继续、关闭后拒绝。单项凭据超过宿主当前额度不会使配置 prepare 失败，而是在请求时返回 503，因此宿主需配置足够预算。

`gatel-http/tests/blocking_config.rs` 验证 KDL 调限、失败候选不改限额、删除配置恢复默认、旧 keep-alive 连接遵守新预算与关闭后提交不复活预算。事务单测另外覆盖过时候选不可发布已准备的调限。

`gatel-http/tests/auth.rs` 通过真实配置事务和 HTTP 请求验证高成本/损坏哈希 prepare 失败、generation 不前进、旧认证继续返回 200。`gatel/tests/passwd.rs` 继续覆盖生成工具与运行时的完整衔接。进程级额度共享由静态池与 Arc 身份测试证明；未宣称完成多租户公平调度、长时间高并发压测、其余密码算法或所有撤销场景。

### 独立实现互操作与解析边界

`gatel-password/tests/vectors.rs` 保存由 PHP 8.4.25 生成的 Argon2id 固定向量，普通测试不需要 PHP 或 Docker；关闭 feature 时明确拒绝该向量。参数矩阵另行覆盖 m/t/p 的边界、整数溢出、缺失/重复参数、盐与摘要长度以及版本、变体和尾随数据，不执行高成本计算。

双向互操作是显式外部门禁，普通测试默认忽略：

```text
cargo test -p gatel-password --features argon2 --test php_interop --locked -- --ignored --nocapture
```

需预先提供镜像 `php@sha256:59fa733c9af643a122f8a9976119460e35ce76dd0a3f2b9c8f75af8e361a54e2`；测试不自动拉取镜像，运行时关闭网络、只读文件系统并限制容器内存与进程数。PHP 验证 Gatel 生成的 bcrypt/Argon2id，Gatel 验证 PHP 的输出，并检查密码首尾空格不能被忽略。2026-09-07 已在本地对 PHP 8.4.25 实际通过；不代表其他版本或全部参数组合已经互操作验收。

2026-09-08 扩展并实测：Windows Rust 对 Linux PHP 8.4.25，两个算法分别覆盖首尾
空格、包含中文/emoji/重音字符的 UTF-8、71 字节 ASCII（bcrypt 生成上限），共
6 个双向夹具组合。双方都拒绝附加字符后的错误密码；含首尾空格的输入也验证去除
空格后失败。密码是双方代码内的固定测试数据，容器参数只传夹具名称，hash 通过
stdin 传递。单个 Rust ignored 测试封装这些组合，不将它报告为 6 个独立测试。
未覆盖全部 Unicode 规范化形式、Argon2 256 字节上限或 bcrypt 旧 72 字节凭据互操作。

`password_policy` fuzz target 与 stable 的 `parser_robustness` 共用解析不变量，验证接受结果一致、长度边界和错误/Debug 脱敏。它们不调用 verify，避免将模糊测试变成攻击者控制的密码计算压力测试；不替代真实 HTTP 认证、资源预算和长期并发验收。
