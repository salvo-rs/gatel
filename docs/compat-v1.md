# v1 → v2 迁移台账（增量审计，尚未完整）

比较基线：本地 `main` 的 `eaf9bde`，检查日期 2026-09-07；没有切换或改写当前 v2 工作树。旧行为以源码为准，不将旧 README 的描述直接当作运行证据。BASE-01 尚未完成：本次逐项覆盖 CLI 定义与两个独立工具入口，配置 parser、默认值和全部示例仍需继续核对，见 ROADMAP。

## CLI 源码映射

旧来源：`main:crates/gatel/src/cli.rs`、`main:crates/gatel/src/main.rs`；新来源：`crates/gatel/src/main.rs`、`src/watch.rs`、`src/trust.rs`、`src/cli_output.rs`。

| 旧命令 / 参数 / 默认值 | 当前 v2 | 差异及后续任务 |
|---|---|---|
| run `-c/--config`，默认 gatel.kdl | run `[CONFIG]`，默认 gatel.kdl，新增 --dry-run | 参数改为位置参数；旧命令行不能原样复制，OPS-01 |
| validate `-c/--config`，默认 gatel.kdl | validate `[CONFIG]`，同默认路径 | 参数 breaking；validate 不绑定端口，run --dry-run 才进行启动准备 |
| reload `-c/--config` 默认 gatel.kdl，`-a/--address` 可选 | 没有独立 reload 命令；run 内监听文件和平台信号 | 旧实现通过 Admin API reload，不是文档所谓单纯发送 SIGHUP；Admin 恢复依赖 CTL-02、OPS-01 |
| serve `[ROOT]` 默认 `.`，`-p/--port=8080`，`-l/--listen=0.0.0.0`，`-b/--browse` 默认关闭 | 已恢复同名命令及短/长参数，新增 --dry-run | 有意 breaking：默认监听 127.0.0.1，外部访问需显式 --listen；IP 地址参数不解析主机名，端口范围 1～65535。复用 v2 file-server，不承诺旧目录列举 HTML 完全一致 |
| trust/untrust `--storage-dir` 可选 | trust/untrust `--directory` 可选 | 参数更名；平台信任库行为需 TLS-04 独立验收，不在本次测试中修改系统信任 |
| completions `[SHELL]` 必填 | 已恢复 bash/elvish/fish/powershell/zsh | 从当前 Clap 树生成，含 v2 命令；命令测试通过，交互安装未全部验收 |
| man-page | 已恢复，并新增可选子命令路径 | stdout roff；支持 run 等独立手册，未知路径退出 2 |
| Windows service install `-c/--config=gatel.kdl` | 缺失 | OPS-02；不自动安装或修改宿主服务 |
| Windows service uninstall | 缺失 | OPS-02 |
| Windows service run（隐藏）`-c/--config=gatel.kdl` | 缺失 | SCM 入口不能用普通 run 的存在替代验收，OPS-02 |
| 顶层 --help/--version | 存在 | 由当前 Clap 生成；版本为当前 workspace 版本 |
| 无对应旧命令 | explain / diff / capabilities / fmt | v2 新增执行顺序、配置差异和当前编译能力查询；fmt 单文件 stdout 格式化或 --check，不展开 import，不替代 v1 adapt |

旧 getting-started 文档只列三个子命令，而 cli.rs 实际还有 serve/trust/untrust/completions/man-page/service。因此后续 BASE-01 同时检查源码与文档，并记录矛盾，不能仅按文档标题建立完整性结论。

## 独立工具

| 旧入口及参数 | 当前状态 | 后续验收 |
|---|---|---|
| gatel-passwd，`-c/--cost=12`、可选位置 PASSWORD，否则 stdin 一行 | 恢复为 `gatel passwd --stdin`，默认 bcrypt cost=12，允许 4～16；新增可选 Argon2id，旧独立二进制未恢复 | 有意 breaking：不接受密码参数或回显式终端输入；stdin 必须 EOF，保留首尾空格，只去末尾一次 LF/CRLF。bcrypt 1～71 UTF-8 字节、Argon2id 1～256 字节，无静默截断。隐藏交互及其余算法仍属于 AUTH-01 / OPS-01 |
| gatel-precompress ROOT，`-e/--encodings=gzip,brotli,zstd`，`-t/--types=html,css,js,json,xml,svg,txt,wasm` | 恢复为 `gatel precompress ROOT`；旧独立二进制未恢复 | 保留主要参数/后缀，新增 dry-run、max-bytes；跳过隐藏项/链接/压缩产物，单 sidecar 原子替换，失败退出 3。采用库默认压缩等级，不保证旧输出字节一致；详见 PRECOMPRESS.md |

来源分别为 `main:crates/passwd/src/main.rs` 和 `main:crates/precompress/src/main.rs`。本轮没有执行旧二进制，因此这里是源码审计，不是差分运行验收。

## 尚需逐项展开

- `main:crates/core/src/config/{types,parse}.rs` 的字段、属性、默认值、类型和范围。
- `main:docs/en` 八份文档与 `main:examples` 全部示例的逐项归属及可运行替代。
- 环境变量自动配置、日志变量、Admin 资源 API、TLS 与监听参数的行为变化。
- 旧新二进制的差分请求、错误退出码及平台场景；QA-02 不能由本表取代。
