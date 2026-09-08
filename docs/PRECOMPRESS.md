# 静态资源预压缩

```sh
gatel precompress ./public
gatel precompress ./public --dry-run
gatel precompress ./public -e gzip,br -t html,css,js --max-bytes 67108864
```

该命令不加载 gatel.kdl、不启动 listener，也不访问网络。默认生成 gzip/Brotli/Zstd
三种 sidecar，分别为 `asset.txt.gz`、`asset.txt.br`、`asset.txt.zst`，可直接用于
`file-server precompressed=#true`。已有同名普通文件会被替换；重复编码（含别名）只写一次。

## 参数与边界

- ROOT 必填且必须是目录，不接受根节点本身为 symlink/Windows reparse point。
- `-e/--encodings` 默认 `gzip,brotli,zstd`；接受 gzip/gz、brotli/br、zstd/zst。
- `-t/--types` 默认 `html,css,js,json,xml,svg,txt,wasm`，大小写不敏感；不带点，只接受 ASCII 字母数字。
- `--max-bytes` 是单个源文件字节上限，默认 67108864（64 MiB），允许 1～1073741824；元数据和实际读取都检查。源文件在内存中形成快照，压缩输出分块写入，不把整个压缩产物另存为内存缓冲。
- `--dry-run` 只检查遍历、大小和输出类型并列出计划，不读取文件内容、不创建 sidecar。
- 跳过点文件/点目录、symlink/reparse point、特殊文件以及 `.gz/.br/.zst`，不会把上次产物再压缩。遍历上限 100000 条目、深度 64；超出返回错误，不无界递归。
- 使用当前压缩库默认质量设置，不保证与旧工具的最高压缩等级或字节输出一致。空文件仍生成合法压缩流。

## 写入和失败

每个 sidecar 使用同目录、独占创建的临时文件。压缩/写入成功后将临时文件 mtime
设为源文件快照的 mtime，再 sync、关闭文件，
重新检查源文件长度/mtime 与目标类型，再 rename。Unix 还同步父目录；输出只继承
源文件的读写权限，不继承执行/特殊权限位。Windows 使用目录继承 ACL。

继承 mtime 使带未来时间戳的构建输入也能生成通过服务端新鲜度检查的产物；
不是简单使用压缩机器的当前时间。设置时间失败与其他 rename 前失败一样保留旧产物。

rename 前失败保留旧产物，并尝试清理自己的临时文件。rename 后的父目录 sync 失败
表示持久性不确定，不是“文件没有改变”。进程强杀可能留下 `.gatel-precompress-*.tmp`，
后续遍历会跳过它们，不自动删除不属于本次操作的文件。

这是**单 sidecar 原子替换**，不是整个资源树或同一源文件三种格式的事务。
遇错立即退出，此前已写好的产物保留。成功为 0、参数错误为 2、I/O/容量/写入错误为 3。
不要在服务中的目录一边替换源文件一边预压缩；推荐在离线构建目录全部成功后再部署。
目录必须受信任且无并发写入者：元数据重检不能防御恶意父目录替换、保留 mtime 的修改，
也不提供源文件与 sidecar 的跨文件原子快照或任意文件系统断电保证。

## 服务时的新鲜度检查

`file-server precompressed=#true` 在编码协商前排除 mtime 早于源文件的 sidecar；
无法取得修改时间也不选择该压缩文件。剩余格式按客户端偏好协商，没有可选压缩文件
时选择允许的动态编码或 identity；全部被拒绝时返回 406。ETag、条件请求、HEAD 和 Range 均基于最终选中的文件表示。
这使更新源文件后遗留的旧压缩产物不会仅因存在就被继续选用。

相同 mtime 允许使用，以支持粗粒度文件系统和统一时间戳的构建产物。这只是元数据
启发式：保留或回拨源文件 mtime、未来时间戳、请求期间并发替换文件均不能靠它保证
内容一致性；服务端不会解压或散列比对内容。仍应在离线目录生成整套产物再统一部署。

## 文件表示协商

内部协商区分 `Coding`、`Identity`、`NotAcceptable`、`Invalid`。
`encode` 在请求进入文件处理器前登记编码和最小尺寸；file-server 将新鲜 sidecar
与可用动态编码一起协商。质量值优先，相同质量先选 sidecar；同一编码有 sidecar
时使用该文件。动态编码保留尺寸及内容类型限制，拒绝 identity 不会强制越过这些限制。
identity 的排除规则依据 [RFC 9110 §12.5.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-12.5.3)。

文件表示在条件请求之前选定：动态编码使用弱 ETag，HEAD 使用同一编码和 ETag，
不计算压缩 body 或报告 identity 的长度；编码表示忽略 Range 并返回整份内容。
没有可接受表示时返回空 406、no-store，不读取文件 body；Vary 包括 accept-encoding，
即使没有启用预压缩。畸形协商字段继续保守地使用 identity，而不是推测有效拒绝。

请求 no-transform 以及 `headers` 配置的最终响应 no-transform/Content-Type
参与动态编码资格判断；响应配置按真实 unwind 顺序预演，外层站点覆盖内层路由。
预先存在的 sidecar 是静态表示，不是动态转换，因此不被 no-transform 禁止。
此协作层限于标准 bundle 内部，尚不是对第三方插件承诺的稳定表示 API。

已执行 Windows 真实 HTTP 的文件/动态编码/预压缩组合、cache 冷热路径、
GET/HEAD/Range/If-None-Match、过期 sidecar 及 headers 覆盖测试。
上述文件及缓存组合在 Linux 容器中也已通过（ROADMAP 第 134 批）；不代表完整
HTTP 套件通过，该批另有 ACME 超时；第 135 批在不改期限的情况下 Linux bundle/HTTP
全 feature/all-targets 重跑通过，历史超时根因仍未确认，原有 ignored 测试仍未执行。
自定义 406 错误页的 GET/HEAD 也已实测：替换正文时清除旧表示元数据并重建长度，
保持 406、Vary 和 no-store。错误说明页不是所请求资源的可接受表示；当前不再对
此说明页重新执行完整编码协商。
仍待扩展：非文件终端（proxy/respond/目录列表）的最终拒绝、任意插件产生的后置
响应策略、定制错误页自身的编码协商、多协议及跨平台矩阵。不能把文件链路验收等同于全部响应闭环。

## 迁移和验证

排查慢运行时，可设置 `GATEL_LOG=gatel::precompress=debug` 和
`GATEL_LOG_FORMAT=json`，阶段事件写到 stderr，不改变 stdout 的产物列表。
`scan` 覆盖目录检查和遍历，`read` 覆盖源检查/读取，`staging` 覆盖临时文件创建，
`encode_write` 覆盖编码及压缩数据写入，`publish` 覆盖权限、时间戳、同步和重命名，
`stdout` 覆盖每个产物列表行的输出。阶段开始/离开事件带阶段名，离开时记录
`elapsed_ms`；事件本身不包含路径或正文，普通错误信息仍可能包含路径。
阶段离开也会在错误返回时记录，不表示发布成功；强制终止、挂起或日志输出自身阻塞
可能只有开始事件。进程启动和编码/文件写入的进一步分解仍需其他诊断。

CLI 集成测试显式打开上述诊断，以专用临时文件捕获 stdout/stderr，避免等待时管道
塞满；普通完成记录 PID 和总耗时。预压缩子进程在 120 秒仍未结束时终止并回收，
失败报告各读取最多 1 MiB 的捕获输出。该上限是测试等待策略，不是生产 CLI 的执行
期限、输出文件大小限制或操作系统异常情况下的强制回收保证。
超时清理另有故障注入：等待 fixture 的标记输出、超时回收及捕获内容经 Windows/Linux
验证，Linux 检查终止信号。此测试只覆盖直接子进程，不是通用进程树监管器。

旧 `gatel-precompress ROOT` 迁移为 `gatel precompress ROOT`，保留主要参数与后缀，
没有新增旧同名独立可执行文件。未知编码不会被吞掉；错误不再返回假成功或写空产物。

测试覆盖真实 CLI 的三种格式还原、空文件、重复运行更新、扩展名/别名、dry-run、
错误退出码、源变化时保留旧产物，以及真实 `gatel run` 的预压缩协商。
Linux 专项检查链接跳过/拒绝，以及 0400/0444/0600/0640/0755/06755 源权限：
产物保留读写位、移除执行与特殊位，源文件权限不变，产物 mtime 与源一致。
包含未来时间戳 HTTP 验证的 6 项集成测试已分别在容器 root 和 UID/GID 65534 下通过；
这不覆盖其他文件系统的时间戳精度或 ACL 语义。
Windows 使用实际目录 junction 验证 reparse-point 分支：根 junction 拒绝，嵌套
junction 不遍历，作为目标时真实写入和 dry-run 均拒绝且保留链接及外部哨兵文件。
测试只移除本次创建的 junction，不递归清理其目标。此证据不覆盖文件 symlink、
所有 reparse tag、并发替换或继承 ACL 的完整权限矩阵。
