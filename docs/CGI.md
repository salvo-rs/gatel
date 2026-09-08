# CGI 协议层与接入路线

状态：协议基础、FastCGI/SCGI 双向交换层与固定脚本 terminal 已实现，共用 gatel-cgi-http 映射和生命周期。真实 TCP 协议后端、两种协议的 HTTP/2 取消与额度归还测试通过；PHP-FPM 8.4.25 的 GET/POST/HEAD 有历史实测记录。Linux 上独立协议后端的 Unix socket 测试已通过，真实 SCGI 应用互操作仍待验收。子进程 CGI 已提供 Windows 可选 HTTP 插件；Linux/macOS 完整子进程调用尚未支持。

## 实现边界

`gatel-cgi-protocol` 仅依赖 `http` 类型，不依赖 Tokio、Salvo、编译器、进程 API 或 HTTP 连接池。FastCGI/SCGI 可选插件及共享 HTTP 适配器按需引入，默认关闭的宿主 feature 控制装配。

- `environment::Environment`：显式构建的确定性环境；禁止重复名称、NUL、非法变量名；总预算最高 1 MiB。不会继承宿主环境，Debug 仅输出名称与总字节数。
- `head::Decoder`：增量 CGI 响应头，最高 64 KiB；支持 LF 和 CRLF，返回精确消费位置，正文留在调用者输入缓冲区。
- `head::BodyLength`：流式正文计数，防止 Content-Length 截断或溢出；HEAD/204/304 不允许正文。适配器必须在发送每个 chunk 前检查，并在正常 EOF 验证长度。
- `fastcgi`：v1 record 借用式解码、padding 跳过、PARAMS 长度编码与跨记录分片；Responder 使用 request ID 1，每连接单请求、不启用 KEEP_CONN。
- `fastcgi::Responder`：检查 stdout/stderr 结束、END_REQUEST、request ID 与累计 stderr 预算。非零协议状态拒绝；application status 单独返回，由适配器决定处理策略。

协议错误后适配器必须关闭并丢弃本次 transport，不能继续把同一会话当作成功请求使用。codec 不自行重试、缓存、不启动后台任务，也不会把 stderr 内容自动写入日志。

## 明确的支持配置

### 后端无关的 HTTP 接入

`backend_directive(BackendFactory)` 是无需 socket 的扩展入口：decode 调用纯语法 `validate`；prepare 解析不可变后端配置并返回 `Backend`，不得连接或启动请求进程；HTTP 请求通过校验和容量控制后才创建 `Invocation` 并调用 `Backend::run`。Invocation 携带输入、确定性环境及绝对 deadline，不含虚构的双向 socket。后端返回的 future 随响应流存活，启动阶段同样处于总期限、panic 和取消边界内。

现有 `directive(Protocol)` 是网络后端便捷入口：内部适配器解析 IP/Unix 目标，在 future 中拨号，仍保留最多 5 秒且不超过总 deadline 的拨号预算，再把连接交给协议 `ExchangeRequest`。原有 FastCGI/SCGI 插件无需改写。共享配置哈希域升级为 v6，防止新旧初始化/生命周期实现被误判为相同配置。

CGI 配置现在通过 SDK 统一 prepare，一次创建后端、处理器及 `Backend::background()` 返回的惰性任务。后端可让请求入口与后台监督器共享同一份资源；宿主按 owner 只准备一次，并在提交后启动任务。默认网络 Backend 不声明后台任务。terminal owner 已修正为所属 route，故不同 CGI 路由的 concurrency 额度互不占用；真实 HTTP 回归验证 `/a` 满额返回 503 时 `/b` 仍成功。

非网络后端测试验证不施加 IP 地址语法、不在 decode 阶段 prepare，以及成功、错误、工厂 panic、启动超时、响应头前取消与响应流丢弃后的资源/额度释放。进程式后端仍必须实现平台级受控启动、stderr 限额、进程树终止和异步回收，只有确认退出结果后才能 finish；这些要求尚未由此接口实现，也不能以 future drop 代替。

CGI 元变量与响应模型参考 [RFC 3875](https://www.rfc-editor.org/rfc/rfc3875)，FastCGI 记录和参数格式参考 [FastCGI 1.0 Specification](https://fastcgi-archives.github.io/FastCGI_Specification.html)。本实现不宣称完整覆盖两个规范。

本项目的响应配置刻意受限：不支持 NPH；无 Status 的绝对 Location 按外部重定向处理；隐式本地重定向明确拒绝，不能绕过正常路由与认证重新分派。重复 Status/Content-Length/Content-Type/Location 拒绝，Set-Cookie 保留多值。HTTP hop-by-hop 和传输分帧字段由宿主管理，CGI 输出这些字段时拒绝。

FastCGI 首批只支持 Responder，不支持 Authorizer、Filter、连接复用、multiplexing 或 management negotiation。接受显式空 stdout 记录，也接受 PHP-FPM 使用 END_REQUEST 隐式结束输出；仍要求 END_REQUEST 的 request ID、状态合法，CGI 头与正文长度正确。不能用 TCP EOF 替代协议完成。

## 后续工作，不是已完成能力

1. **双向流交换验收**：已有 HTTP 流式适配；迟到错误通过保留最后一个字节直至 END_REQUEST 成功验证，避免提前满足 Content-Length；仍需继续扩展客户端关闭与 middleware 组合矩阵。
2. **请求映射扩展**：固定 script/root 和可选 remote-root 已实现；继续评估显式动态脚本选择。当前请求路径仅形成 PATH_INFO，绝不决定 SCRIPT_FILENAME。
3. **独立 FastCGI 包验收**：已实现类型化配置、TCP/Unix dial、容量控制、流式响应与 feature 注册；PHP-FPM TCP 基础链路已实测，继续 Unix 和跨平台测试。
4. **SCGI 与子进程 CGI**：复用相同环境与响应解析器，但 transport/lifecycle 分别实现。子进程需要额外的退出码、stderr、kill/reap 和平台进程树保证；不能仅依靠 abort future。
5. **互操作**：独立协议后端及真实 PHP-FPM 验证，故障注入覆盖短记录、错误 ID、无 END_REQUEST、响应截断、慢上传/下载、提前拒绝与取消。

## 当前证据

`cargo test -p gatel-cgi-protocol`：固定 BEGIN_REQUEST 字节向量、每个 header 分割位置、大块正文不占 header 预算、70 KiB 参数跨记录、注入/重复字段、长度边界、stderr 限额、提前 EOF、错误记录和 request ID。测试不依赖编解码器自我 roundtrip 作为唯一证据。

## FastCGI 异步交换层

`gatel-fastcgi::exchange::run` 消费一个 transport 与一个精确长度的请求输入。CONTENT_LENGTH 必须与声明长度一致，上传超限/短读/多余字节均拒绝。交换层不接受未知长度；HTTP 适配器可显式启用有界缓冲，将未知长度转换为准确长度。

交换层并行上传 stdin 和接收 stdout/stderr。输出通过 `Output::head/body/finish` 施加背压，每个 record 的内存至多约 66 KiB，上传块为 8 KiB。stderr 只累计字节数，既不转发给客户端也不自动记录。完整提前响应会取消未完成的上传；每次交换独占 transport，函数返回或 future 被丢弃即关闭其持有的两半，不会遗留后台 writer。

`Limits::overall` 覆盖整个异步交换；`idle` 是单次 I/O 或输出回调的等待上限，不是底层每个 TCP packet 的滑动空闲计时。首次响应记录可等待整体期限，避免将正在推进的慢上传误判为响应空闲。需要更精细的跨方向活动计时可以随后替换，不可将当前语义描述成已经实现的 packet-level idle 策略。

只有响应头合法、正文长度正确、stdout 关闭、END_REQUEST 到达且 application status 为零，才调用 `Output::finish`。如果已经发送响应头后发生错误，HTTP 适配器必须将错误交给响应流并中断它，不能补写第二个状态或伪造正常 EOF。Display 错误不包含 stderr 或任意 backend 错误文本。

`cargo test -p gatel-fastcgi` 当前覆盖 32 字节双向缓冲下的大上传、提前拒绝取消上传、无 END_REQUEST、应用错误退出、整体超时释放 transport、外部取消同时释放输入与 transport、长度不一致、慢上传和下游背压。`gatel-http/tests/fastcgi.rs` 另验证真实 TCP terminal、请求环境、上传和 HEAD。PHP-FPM 的显式测试证据见下节，Unix socket 尚未实测。

## 固定脚本 terminal

```console
cargo run -p gatel --features fastcgi -- validate plugins/fastcgi/examples/gatel.kdl
cargo run -p gatel --features fastcgi -- run plugins/fastcgi/examples/gatel.kdl
```

需要先自行启动目标 FastCGI 后端；Gatel 不启动或管理 PHP-FPM 进程。示例脚本与配置位于 `plugins/fastcgi/examples/`。最小指令为：

```kdl
fastcgi "127.0.0.1:9000" root="./public" script="index.php" script-name="/index.php" server-name="example.test"
```

backend 只接受显式 IP:port；Unix 系统可使用 `unix:/run/php/php-fpm.sock`，Windows 明确拒绝 Unix 配置。root 相对配置目录，script 必须是 root 内的相对文件；prepare 解析真实路径并拒绝 symlink 指向 root 外部。脚本文件和目录必须由受信任部署者控制：FPM 在稍后重新打开路径，这不是对恶意文件系统写入者的 TOCTOU 防护。

所有匹配路由请求执行同一固定 script。`/index.php/info` 映射 PATH_INFO `/info`，其他路径完整映射为 PATH_INFO；编码路径只解码一次，拒绝 NUL、控制字符、嵌套转义、编码分隔符和 dot traversal。SERVER_NAME 来自配置而非 Host。SERVER_PORT 来自 listener，REMOTE_ADDR 使用宿主已处理可信代理的地址。

固定 script 是部署者的显式选择，不等同于静态文件的公开目录扫描：允许明确配置
根内 `.app/front.php` 等隐藏脚本，也允许指向根内固定脚本的链接；请求不能借助
PATH_INFO 改选 SCRIPT_FILENAME。不要把静态文件的隐藏名过滤作为 CGI 的授权策略。
共同边界测试覆盖 URI 精确匹配/路径后缀/相似前缀/其他脚本名称/Unicode、隐藏固定
脚本、remote-root 仍需本地普通文件，以及 Unix 根外/断链拒绝和根内链接允许。
remote-root 保留配置中的相对脚本名，不将本地链接的最终目标路径猜测映射至远端。

Authorization 默认不传递，可显式设置 `pass-authorization=#true`。HTTP_PROXY、hop-by-hop 及 Connection 指定头不进入环境；带下划线的请求头忽略，防止与连字符规范化后的环境名称碰撞。

多值字段规则：Cookie 按原顺序用 `; ` 合并，普通可合并头用 `, ` 合并；Content-Type、Host 和启用传递后的 Authorization 重复时拒绝映射并在连接后端前返回 400。默认不传递的 Authorization 直接丢弃，不因重复而暴露或拼接凭据。Connection 字段逐行校验 token；其中指定的 Content-Type 连特殊变量 CONTENT_TYPE 也不生成。CONTENT_LENGTH 始终来自已确认的上传长度，而不是客户端原始字符串或 Connection 指定值。

`concurrency` 默认 32，作用于该指令的兼容状态，满时返回 503；`max-body` 默认 16 MiB，超过上限返回 413；`timeout` 默认 60 秒，覆盖缓冲、连接和交换的同一个总期限，连接另有最多 5 秒的子期限。更改 concurrency 会重建容量状态，旧 generation 排空期间可能与新额度重叠，不应理解为全进程 FastCGI 总并发上限。

已知长度请求始终流式上传。未知长度/chunked 默认返回 411；配置 `buffer-unknown=#true` 可启用内存缓冲，`max-buffer` 默认 1 MiB，有效正文上限为它和 `max-body` 的较小值。缓冲前获取并发额度，超限返回 413、读入超时返回 408、读入错误返回 400，且不会连接后端。只有读到完整 EOF 才生成 CONTENT_LENGTH 并开始 FastCGI 交换；无磁盘落盘。正文容器与输入框架有额外分配开销，不能把正文上限直接当作进程 RSS 上限。

回归测试 `chunked_buffer_is_opt_in_bounded_and_releases_capacity_after_timeout` 通过真实 HTTP/TCP 验证默认拒绝、超限和超时不访问后端、超时释放唯一并发额度，以及分块正文转换为正确 CONTENT_LENGTH/STDIN。

`gatel-http/tests/fastcgi_h2.rs` 使用真实 TCP 上的 Hyper HTTP/2 客户端与独立 FastCGI 测试后端，验证无 Content-Length 的多帧上传生成准确 CONTENT_LENGTH/STDIN 和 SERVER_PROTOCOL。同一 H2 连接的另一个 stream 在唯一额度占用时得到 503；分别在收到部分响应后、等待响应头期间取消首个 stream，后端 socket 及时关闭，后续 stream 恢复成功。两项测试有 10 秒外层期限，而后端超时配置为 60 秒，避免把超时回收误当成取消传播。此证据不覆盖 TLS ALPN、GOAWAY 或不同 H2 客户端实现。

响应使用有界单消息通道与随响应体轮询的交换 future，不 spawn 后台任务；客户端丢弃响应体即丢弃交换和 permit。HEAD/204/304 以及 Content-Length 为零的响应，在返回头部前完成协议验证。有正文时只保留最后一个字节，不缓冲完整响应；成功 END_REQUEST 后才交付末字节。连接/协议故障在发送响应头前返回 502，初始超时返回 504；发送头之后通过流错误关闭响应。

## SCGI 协议层与可选 terminal

`gatel-cgi-protocol::scgi::headers` 已实现请求环境 netstring 编码，依据 [SCGI 原始协议](https://python.ca/scgi/protocol.txt)。调用者必须提供准确正文长度及一致的 CONTENT_LENGTH；编码器将其排在首位，随后写入唯一的 SCGI=1，其他环境键稳定排序。未知长度策略由未来 HTTP 适配器处理，不在编码器中猜测。头预算涵盖 NUL 分隔符与自动加入的 SCGI 字段，netstring 外层使用无前导零的十进制字节长度。

4 项独立回归覆盖原文 70 字节头向量、缺失／非数字／溢出／不一致长度、SCGI 标记校验及 UTF-8 字节预算。SCGI 本身不规定响应格式；本项目采用既有受限 CGI 响应头配置，以及独立的 EOF 完成、上传／下载背压及取消实现，不能套用 FastCGI 的 END_REQUEST 或 stderr 记录。当前已提供默认关闭的 `scgi` host feature 和同名指令，CGI-03 仍在实施中。

宿主示例：`cargo run -p gatel --features scgi -- run plugins/scgi/examples/gatel.kdl`，需自行启动 127.0.0.1:4000 的 SCGI 后端。参数与 FastCGI 共用：显式 IP:port／Unix 地址、固定 root/script、script-name、remote-root、concurrency、max-body、buffer-unknown、max-buffer、timeout、pass-authorization。当前固定脚本配置要求本地 root/script 存在，SCGI 应用可用部署 marker 表达该路径；Gatel 不执行该文件，也不根据请求选择文件。

`gatel-cgi-http` 集中管理映射、输入缓冲、并发 permit、dial、总期限、响应通道与取消；两个协议插件只适配各自交换层。这是 CGI 专用适配器，不是任意协议的通用 transport 或沙箱。错误响应正文统一为 CGI 通用消息。真实 HTTP/TCP 回归覆盖 SCGI chunked 上传、PATH_INFO、Authorization 默认过滤及 HEAD；Unix 和 SCGI 应用服务器互操作仍待验收；HTTP/2 与 reload 证据见下文。

## SCGI 异步交换层

H2 热更新增量证据：`scgi_h2_reload_preserves_connection_generation_and_failed_prepare` 验证旧 CGI 流暂停时，缺失脚本导致 prepare 失败并保持旧配置；成功 reload 后，同一旧 H2 连接的新 stream 仍用旧配置，而新 H2 连接用新配置。后端 EOF 释放保留的最后一字节，旧响应完整结束；旧 generation 在旧连接关闭后释放，新连接仍能继续请求。这遵循当前“连接固定 generation”的架构，不是逐 stream 切换配置。长连接上的配置强制退役策略仍需单独设计。

HTTP/2 增量证据：`gatel-http/tests/scgi_h2.rs` 独立解析 SCGI netstring 与环境字段，验证无 Content-Length 的两帧上传被有界缓冲为准确 CONTENT_LENGTH=5、SERVER_PROTOCOL=HTTP/2.0 和 hello 正文。唯一并发额度占用时，同一 H2 连接的第二个 stream 得到 503。分别在响应头之前和部分正文之后取消首个 stream，后续请求在同一连接成功返回 ok。

SCGI 上传 EOF 是半关闭，不作为取消证明；测试在确认额度复用后探测旧连接输出方向，要求写入失败。后端首个响应从不主动完成，总测试期限 10 秒短于后端默认总期限；不能靠后端超时或正常 EOF 让测试误通过。共享适配层无需修改即通过这两项测试，此批属于补充验收，不是新增子进程执行能力。

热更新增量证据：`scgi_response_pins_old_generation_and_failed_prepare_keeps_lkg` 验证 H1 在途响应保留旧 generation，准备失败保持旧配置，成功更新后新连接使用新配置，旧响应完成后 generation 被释放。此项已完成；SCGI H2 复用已有下述取消/额度测试，H2 热更新也已通过上文测试，真实应用服务器的 reload 验收仍待进行。

独立 `plugins/scgi` 包现提供 `exchange::run`，交换核心使用 Tokio 与纯 CGI 协议包，HTTP 接入使用共享 gatel-cgi-http 和插件 SDK，不依赖 FastCGI、编译器或 HTTP 连接池。一次交换拥有连接与输入，双向并行；准确长度的上传结束后半关闭写端。响应采用本项目受限 CGI 头配置，以后端 EOF 验证完成；无 END_REQUEST、应用退出码或 stderr 通道。保留正文最后一个字节直到 EOF 与长度校验成功，输出回调施加背压。

整体期限覆盖交换；每次 I/O 和输出回调另受 idle 上限约束，首次响应可等整体期限，以容纳持续推进的上传。完整提前响应取消尚未完成的输入。丢弃 future 会关闭其拥有的连接和输入，无 detached writer。

`cargo test -p gatel-scgi` 当前 7 项验收：32 字节 duplex 缓冲下 100 KB 双向传输、截断／超长／HEAD 非法正文拒绝、超时关闭 transport、提前拒绝取消等待输入、外部取消释放输入与连接、输出回调停滞触发 idle 超时，以及真实 TCP 后端验证固定请求字节、上传半关闭后返回响应。TCP 后端逐字节写响应，但不假定网络保留写入边界。宿主配置／feature 和基本 HTTP 流适配已接入；真实应用服务器和 Unix 互操作仍待验收。

## 子进程插件的剩余生命周期工作

`plugins/cgi` 已包含独立交换核心、代际监管队列和 Linux/Windows 进程原语。交换器接收拥有所有权的 stdin/stdout/stderr、请求输入及监督器提供的退出状态 future，不自行启动程序；平台原语负责实际启动。Windows 已组合为可监管 Invocation，并通过可选 `cgi` feature 接入宿主；Linux 的完整调用适配仍待实现，默认宿主生产依赖树不包含该包。

交换同时推进上传、输出、stderr 和退出等待。stdin 精确校验长度并关闭写端；完整提前响应可取消尚未完成的上传。stdout 使用共享 CGI 头/正文长度校验，最后一个字节保留至 stdout EOF、stderr EOF 和成功退出全部满足，再调用 finish。非零/信号退出、stderr 超限、截断/超长响应或期限耗尽均不 finish。stderr 只计数并丢弃，默认上限 64 KiB，不存储或记录其内容；安静的 stderr 不单独触发 idle 超时，整个调用仍受 overall 上限约束。

8 项测试覆盖 32 字节管道下 100 KB 上传/响应与 stderr 并发、EOF 后等待退出和失败退出、stderr 限额及截断/HEAD 非法正文、安静 stderr 与退出等待总期限、取消释放所有管道/输入/退出等待，以及上传长度错误。额外验证成功退出仍等待 stderr EOF，以及 head/body/尾字节/finish 四阶段的输出故障、空闲超时和总期限优先到期。这些是内存管道和受控退出状态的协议测试，不是 OS 子进程 kill/reap 验收。

宿主已提供显式协作关闭信号和共享宽限期，子进程 CGI 仍需完成以下端到端集成（下文记录已完成的组件）：

1. 宿主已有有界的 generation 退役作用域，支持旧配置释放后的异步任务清理；继续在此基础上接入每次子进程调用的 kill/reap。清理期间必须继续占用进程配额，避免取消风暴绕过并发上限；目前的 generation 名额不是进程配额。
2. 固定可执行文件/参数、canonical root/script、显式环境白名单与 stderr 字节上限；无 shell 插值，请求字段不能选择程序。
3. Windows Job Object / Unix 进程组或更强平台隔离，启动失败、取消、期限耗尽及主进程先退出时均终止剩余受控进程并等待回收。Unix 子进程自行脱离进程组仍是单独的隔离边界，不能宣称任意恶意程序都被沙箱覆盖。
4. stdout EOF、正文长度与进程退出结果共同决定响应成功；finish 之前确认状态。验证孙进程、非零退出、stderr 超限、客户端断开、热更新和宿主退出。

已审阅并仅在 Windows 目标引入 [process-wrap 10.0.0](https://docs.rs/process-wrap/10.0.0/process_wrap/)，锁定精确版本以保持当前挂起/分配 hook 顺序；不使用其阻塞 wait 路径，不能用依赖名称替代本项目的生命周期测试。

## 进程监管队列

`gatel-cgi::supervisor` 提供代际拥有的有界监管队列：prepare 仅构造 Client/Supervisor，`background()` 返回协作退出任务，不自行 spawn。单代容量 1～1024 同时计入排队、运行和清理；满载立即返回 Busy，不建立额外等待队列。请求 future 丢弃只取消 run，worker 继续拥有 Job 并等待 cleanup；回收前不归还额度，也不发布 execute 的完成结果。排队阶段已经取消的 Job 不启动。

退出先关闭本代接收入口，再并行取消/清理所有运行任务。run 的 panic 转为错误并进入清理；cleanup 错误或 panic 立即关闭共享进程配额池，然后取消本代其余工作，不允许其他兼容代用未经确认已回收的额度启动替代进程。强制 abort 丢弃已开始的 Job 时同样关闭共享池，且先于归还额度；未开始的候选销毁、正常完成或正常退役不关闭共享池。cleanup 不受客户端取消打断，最终受宿主协作退出宽限和强制 abort 策略约束。

9 项监管测试用可控 Job 验证取消后的配额保留、惰性准备/排队取消、并行退出清理、执行 panic/清理失败关闭、未激活销毁、清理后交付结果，以及跨代配额和强制中止，不启动真实 OS 进程。Job 是受信任的 Rust 实现契约：构造必须惰性，cleanup 必须实际终止并回收进程作用域，Drop 必须提供强制 abort/runtime 销毁时的同步紧急终止。队列自身不能证明这些平台动作已发生；Windows 的实际调用与宿主接入由后续两节的独立测试验证。

## Linux 进程组原语

`gatel-cgi::process::Process` 是 Linux 专用底层实现，尚未接为监管 Job。启动要求显式绝对可执行路径和工作目录，参数不经过 shell 展开，环境完全清空后仅写入经过校验的 Environment；stdin/stdout/stderr 独立管道，程序成为新进程组的组长。

`exited()` 用 `waitid(WEXITED | WNOHANG | WNOWAIT)` 观察退出，不立即回收组长；`terminate_and_reap()` 先向进程组发送 SIGKILL，再 await 直接子进程。保留 waitable 组长直到最后一次组信号，避免在回收后向可能被复用的数字 PGID 发信号；Drop 提供同步组终止和 Tokio 的直接子进程回收后备，不创建独立阻塞 waiter。[waitid 的 WNOWAIT 语义](https://www.man7.org/linux/man-pages/man3/waitid.3p.html) 是这一顺序的依据。

调用方不能用 waitpid(-1)、忽略 SIGCHLD 等方式从外部抢先回收该子进程。它不承诺回收非直接后代，也无法约束逃离组的程序：完整平台 Job 仍需要更强的进程作用域完成证明，不能仅凭发送 SIGKILL 就释放“整棵进程树”的配额。macOS 尚无对应实现；Windows 使用下面不同的 Job Object 完成边界。

2026-09-07：Linux Docker 的 `plugins/cgi/tests/process.rs` 5 项真实进程测试通过，完整 workspace/all-features/locked 回归也通过。测试覆盖 /usr/bin/env 的完全独立环境、退出码 7 的重复非消耗观察、取消/Drop 后继承管道的后代结束，以及直接子进程已回收的 ECHILD 证据；不以管道 EOF 推导全部后代已回收。

## Windows Job Object 原语

Windows 的 `gatel-cgi::process::Process` 使用 `process-wrap =10.0.0` 和本项目 `gatel-process-windows::Job` 的安全接口。前者挂起创建、分配内部 Job 再恢复执行；post_spawn 阶段先加入可查询的外层 Job，因此执行前两个 Job 都已关联。外层设置进程数量限制并查询整个嵌套作用域的活动计数，内层用于显式终止；两者都配置 kill-on-close，不允许 breakaway。平台 FFI 隔离在独立包中，CGI 包继续 forbid unsafe，不依赖第三方私有句柄。

`exited()` 只轮询非阻塞根进程状态；`terminate_and_reap()` 终止 Job、取得根退出状态，并持续查询到外层 Job 活动进程为零才成功返回。父进程已退出不代表完成，清理 future 被取消后仍可重试；Drop 提供紧急终止。没有调用 process-wrap 可能创建阻塞 completion-port waiter 的 wait 方法。

启动仅接受绝对 `.exe` 和绝对 cwd，禁止 cmd/bat 的隐式 shell 分派；环境不继承宿主变量，非 UTF-8 值在创建进程前拒绝。默认每次 CGI Job 最多 64 个活动进程（包含根进程和受控后代）；KDL `process-limit` 和底层 spawn_with_limit 可选 1～1024。上限由 Windows 在创建/分配进程时执行，不依赖用户态计数后才拒绝。该原语仍不是恶意代码安全沙箱。

原 win32job 依赖已移除，改用固定大小 JOBOBJECT_BASIC_ACCOUNTING_INFORMATION 查询，消除 PID 列表容量约 1024 的限制；查询失败绝不当作计数为零。平台包私有 OwnedHandle 负责唯一关闭，每处 FFI 有局部安全说明，不导出原始句柄，也不依赖 Tokio、HTTP 或配置层。创建失败的进程可能暂时仍计入 Windows accounting，因此只将清理后活动计数为零作为完成条件，不根据根进程退出时的瞬时计数推断剩余进程数量。

2026-09-07：Windows 5 项真实进程场景和 1 项子进程夹具入口通过，覆盖环境隔离/退出码、父进程先退出仍清理后代、取消与 Drop、拒绝 CREATE_BREAKAWAY_FROM_JOB、拒绝隐式 shell/非法环境。其上的调用适配与宿主 feature 接入见后续两节。

实现依据：[process-wrap 的挂起分配契约](https://docs.rs/crate/process-wrap/10.0.0)、[Windows 进程数量限制](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information)、[Windows 活动进程计数](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_accounting_information)。平台边界按 Win32 文档实现，未移植 win32job 源码。

## Windows 完整调用适配

`gatel-cgi::job::{Program, Request, Invocation}` 将平台进程、交换和监管连接起来。Program 是来自受信任配置的不可变程序/参数/cwd；每次 Request 拥有环境、输入、输出、长度及绝对 deadline。构造惰性，交给 supervisor 后才启动；请求超预算或 deadline 已过时在启动前拒绝，排队和启动消耗不延长绝对期限。

退出状态 future 先观察根退出，再终止剩余 Job 进程并等待活动数归零，之后才允许交换器发布最后一个正文字节和 finish。失败、取消、panic 仍由 supervisor 持有 Invocation 并执行 cleanup；cleanup 最多 2 秒，不能确认回收时返回错误并封闭该代接收入口，而不是继续启动替代进程。Drop 仍提供平台紧急终止后备。

`process_job` 是无默认测试 harness 的真实集成程序，同时作为 CGI 可执行夹具，避免测试框架前导文本污染 CGI stdout。2026-09-07 Windows 验证 100 KB 回显、退出码 7、stderr 超预算、挂起超时、启动前过期、请求取消及清理后的再次成功调用。取消后的配额恢复用不能启动程序的校验探针确认，不以固定 sleep 推断已回收。非 Windows 的该测试入口为空，不算跨平台调用验收。

该适配已通过 BackendFactory/bundle 接入共享 HTTP 映射；Linux 原语不能证明全部后代完成，因此尚不导出这一调用适配。Windows 后代数量上限已配置化；改变 concurrency 时的跨代预算调整及更多生命周期组合仍待完善。

## 使用 Windows 可选 CGI 插件

构建 `cargo build -p gatel --features cgi`，配置参考 `plugins/cgi/examples/gatel.kdl`。终端指令为 `cgi "app.exe" root="." script="app.exe" script-name="/app"`。程序相对配置目录解析并 canonicalize；工作目录来自 root；script 仍由共享层做固定路径映射和 containment 校验。程序无附加命令行参数，必须通过 CGI 环境（含 SCRIPT_FILENAME）获知脚本与请求，不能把请求字段拼接为参数。remote-root 不适用于本地进程，会在 decode 阶段拒绝。

concurrency 范围 1～1024（默认 32）。HTTP 与进程分别使用两个同容量池；后者覆盖排队、运行、清理。同一指令所有者且 concurrency 未改变的配置通过状态注册表复用进程池，即使程序或其他设置改变，旧代清理仍占用新代可见的容量；各代队列和退役标记独立。改变 concurrency 或所有者会建立新池，不承诺这些不兼容配置重叠期的合计上限。

容量不足、关闭/退役返回 503，超时 504，程序/协议失败 502；若正文已开始则中断流，不重新发送状态。超时响应可能先于清理完成，期间后续请求继续收到 503，不能提前复用进程额度。未确认清理成功的共享池会保持关闭，兼容重载不自动恢复，需先查明进程回收失败；不能靠重复提交同配置清除故障。

`cgi_capacity` 通过真实 prepare/commit/reject 事务检查池的 Arc 身份、旧额度阻塞新代、失败候选保留活动池，以及 concurrency 改变时明确新建池。共享层配置指纹为 v9，容量状态域为 v2；第三方网络 BackendFactory 默认忽略进程池，进程后端通过 prepare_with_capacity 接收宿主池。

`process-limit` 是每次调用的进程总数上限，不是并发调用数；设为 1 时不允许创建后代。它经 BackendConfig → Program → spawn_with_limit 传递，在执行程序恢复运行前设置。修改值会重新准备配置，但保留兼容并发池；已运行的旧调用继续使用旧上限，不追溯修改。FastCGI/SCGI 不管理远端进程，任何显式 process-limit（包括默认值 64）均拒绝。第三方 BackendFactory 应校验此可选字段，不支持时明确拒绝。

Windows HTTP 回归使用 process-limit=2，验证根进程可创建一个子进程、第二次创建失败，随后清理后代才返回成功；另覆盖非法类型/范围保留旧配置、1/1024 边界及仅修改此字段触发 reload。单元测试验证配置指纹变化但容量兼容性不变。

Windows `gatel-http` 的无 harness `cgi_process` 测试通过真实 HTTP GET/POST/HEAD、失败退出/响应头前退出 502、超时 504、清理后恢复、无效程序 prepare 保留旧配置、替换为普通路由和宿主退出。非 Windows 构建可以启用 feature 并查看能力，但配置明确报平台不支持；不静默退回只终止父进程的实现。

## Unix socket 传输验收

`crates/gatel-http/tests/cgi_unix.rs` 在 Unix 平台运行真实 HTTP → Unix socket 链路，FastCGI 与 SCGI 各一项。相对 `unix:cgi.sock` 按配置目录解析；socket 尚不存在时返回 502，随后启动后端，在同一代配置、concurrency=1 下成功处理 POST，验证失败拨号归还额度且无须重载。FastCGI 检查 BEGIN/PARAMS/STDIN 和 END_REQUEST 完成；SCGI 检查 netstring、实际正文长度以及上传半关闭后仍可读取响应。

2026-09-07 已在 Linux Docker、Rust 1.98.0 下实际执行上述两项并通过；Windows 按 `cfg(unix)` 跳过。复验命令：`cargo test -p gatel-http --test cgi_unix --locked`。这些后端是独立测试协议端，不等同于真实 SCGI 应用服务器或 PHP-FPM 的 Unix socket 互操作认证。

## 请求体错误契约

预缓冲超时补充验收：`buffer_idle_timeout_releases_capacity_without_dialing_backend` 使用真实 HTTP 的 100-continue 确认开始等待正文，在 timeout=30 / idle-timeout=1 下暂停上传；唯一并发额度占用时返回 503，空闲期限返回 408 后后续 GET 成功，后端仅收到该有效 GET。虚拟时间单测验证持续进展会刷新单次读取等待，却不会延长外层总期限。

CGI 环境映射在获取上传并发额度和读取正文之前校验。未知长度上传先以临时 CONTENT_LENGTH 构建环境，完整缓冲后再用 `Environment::replace_existing` 写入实际长度；该操作只更新已有变量，预算或 NUL 校验失败不改变旧值或计数，普通 insert 仍拒绝重复名称。`cgi_admission.rs` 验证只发 chunked 请求头而不发正文时，非法路径、重复 Content-Type/Authorization 立即得到 400，不连接后端，随后有效请求仍能取得 concurrency=1 的额度。

FastCGI／SCGI 均支持 `idle-timeout`（秒，默认 15，范围 1～3600），控制交换层单次输入读取、网络 I/O 和输出回调的等待。它不是每个 TCP packet 的活动计时器；首次响应仍可等待总期限，以容纳持续推进的上传。`timeout` 是缓冲、连接和交换的总上限，增大 idle-timeout 不会延长它；未知长度预缓冲的每次正文读取也受 idle-timeout 约束，同时仍受总期限限制；上传等待超时返回 408，而后端交换超时返回 504。真实 SCGI 回归用 `timeout=30 idle-timeout=1` 验证后端输出头后不关闭连接，在总期限之前得到 504。

SCGI 上传补充验收：`short_and_excess_input_fail_and_close_transport` 核对短读失败和多余字节不转发；`declared_budget_failure_sends_nothing` 验证声明超预算时连接不发送任何请求字节。交换层测试现为 9 项。完整提前响应允许取消未完成上传，与上述后端仍等待响应的失败场景分开处理。

配置校验补充：remote-root 的绝对 POSIX 路径语法在 decode/validate 阶段检查，文件存在性与 containment 仍在 prepare 检查。script 额外拒绝控制字符、重复斜杠、尾斜杠和显式 `.` 片段；server-name 拒绝控制字符。FastCGI 与 SCGI 使用相同规则。

宿主全局正文限制和标准 `body-limit` 中间件现在产生 SDK 的 `BodyTooLarge` 类型标记。FastCGI 在未知长度缓冲阶段保留该分类，返回 413 而非普通读取错误的 400，并在连接后端之前结束请求。`BodyTooLarge::find` 识别 Salvo/I/O 包装与标准 source 链，最多检查 32 层，不按错误字符串猜测原因。真实 TCP 组合回归同时覆盖全局与路由限制；此项不代表其他 terminal 的所有读取错误都已统一分类。

## 实测：Windows Gatel → Linux PHP-FPM

2026-09-09 复验：仍使用下述固定 PHP 8.4.25 镜像，Windows 经 localhost 临时端口、Linux
测试容器经同一后端的网络命名空间，两侧的协议交换和 HTTP GET/POST/HEAD 两项 ignored
测试均显式执行通过（共四次）。只复制仓库示例脚本，测试后删除本次容器，镜像保留。
同日 Windows 的 gatel-password/PHP 双向 bcrypt、Argon2id 互操作测试通过，覆盖空格、
Unicode、71 字节边界六组组合。该证据不扩展为其他 PHP 版本、Unix socket 或生产 FPM 配置验收。

2026-09-07：Docker 中的 PHP 8.4.25 (`fpm-fcgi`)，镜像 `php@sha256:59fa733c9af643a122f8a9976119460e35ce76dd0a3f2b9c8f75af8e361a54e2`；仅发布到 localhost 的临时端口。Gatel 与测试在 Windows 运行。示例脚本复制到容器 `/app/index.php`，无须共享宿主文件系统。

`remote-root="/app"` 只替换发给后端的 DOCUMENT_ROOT/SCRIPT_FILENAME 前缀，本地 root/script 仍做存在性与 containment 检查。remote-root 要求无 traversal 的 POSIX 绝对路径，由部署者保证远端内容与本地脚本对应；不通过探测远端文件系统验证。

设置 `GATEL_PHP_FPM_ADDR` 为明确的测试地址后，执行：

```console
cargo test -p gatel-fastcgi --test exchange real_php_fpm_transport -- --ignored
cargo test -p gatel-http --test fastcgi real_php_fpm_get_post_and_head -- --ignored
```

两项均实际通过。后者包含非空 POST 上传/回显及 HEAD 无正文。测试默认忽略，避免普通 cargo test 向未知外部后端发送请求。首轮实测发现空 stdout 记录缺席导致误拒绝；已按真实 PHP-FPM 行为修正并增加不依赖外部服务的回归用例。可对照 [PHP 官方 FastCGI 实现](https://github.com/php/php-src/blob/PHP-8.4/main/fastcgi.c)。测试容器用毕删除，镜像保留在本机缓存。

2026-09-08 复验：同一固定镜像下，Windows 宿主经 localhost 临时映射端口、Linux
测试容器经后端共享网络命名空间，两侧分别通过上述两项测试（共 4 次）。后端仅
使用本次复制的示例脚本，测试容器用毕停止并删除。此证据仍是 TCP FastCGI，
不涵盖 PHP-FPM Unix socket、生产配置、扩展模块或远端部署目录一致性。
CI 新增 `fastcgi-interop`：固定镜像、复制夹具、等待 FPM 就绪、显式运行两项 ignored
测试、失败日志与 always 清理；配置已落地，但尚未推送或取得远端执行结果。
