# Gatel Code Improvement Report

审查基线：`main` 最新提交，新分支 `chris/project-improve-audit`。审查范围覆盖 Rust workspace、核心代理/认证/配置解析代码、示例配置、打包脚本和文档。基础验证结果：`cargo check --workspace` 通过，`cargo clippy --workspace --all-targets -- -D warnings` 通过，`cargo test --workspace` 通过。

## 已完成

- [x] 自动生成 KDL 配置时缺少字符串转义。
  - 位置：`crates/core/src/config/parse.rs::auto_config_from_env`、`crates/gatel/src/main.rs` 的 `serve` 子命令。
  - 风险：环境变量或 CLI path 中的 `"`、换行等字符会破坏生成的 KDL，严重时可插入额外配置指令。
  - 改进：新增 `kdl_string` 统一生成 KDL 字符串字面量，并用于环境变量配置和 `serve` 命令合成配置。

- [x] Basic auth / forward proxy auth 的 Base64 解码过于宽松。
  - 位置：`crates/core/src/encoding.rs`、`crates/core/src/proxy/forward_proxy.rs`。
  - 风险：旧实现遇到 `=` 后直接停止，`SGVsbG8=bad` 这类带尾随垃圾的数据仍可能被接受；forward proxy 还维护了一份重复解码器。
  - 改进：严格拒绝 padding 后追加数据、非法 padding 长度和非 Base64 字符；forward proxy 复用核心解码器，避免两套认证解析行为漂移。

- [x] `encoding` 模块使用模块级 `#![allow(dead_code)]` 掩盖未使用代码。
  - 位置：`crates/core/src/encoding.rs`。
  - 风险：模块级豁免会让后续真正的死代码继续累积。
  - 改进：删除未接入的 `percent_encode` 和 `html_escape`，移除模块级 `dead_code` 豁免。

## 第二轮已完成

- [x] `gatel reload --address` 手动指定 admin 地址时不会自动带 token。
  - 位置：`crates/gatel/src/main.rs::Commands::Reload`。
  - 影响：使用 `--address` 时即使配置文件中有 `admin-auth-token`/`admin-write-token`，请求也不会带认证头；这可能导致安全配置下 reload 命令意外失败。
  - 改进：新增 `reload_target`，保留 `--address` 覆盖地址，同时仍从配置读取 `admin-auth-token` / `admin-write-token`。

- [x] `reload_config` 在 Windows 构建路径下被 `#[allow(dead_code)]` 保留。
  - 位置：`crates/gatel/src/main.rs::reload_config`。
  - 影响：该函数仅 Unix SIGHUP 路径使用，Windows 上属于条件编译导致的死代码。
  - 改进：用 `#[cfg(unix)]` 约束热重载函数，用 `cfg_attr` 只在非 Unix 下允许 signal handler 参数未使用。

- [x] FastCGI 记录头读取后没有校验版本号和 request id。
  - 位置：`crates/core/src/proxy/fastcgi.rs::read_record_header` / response loop。
  - 影响：当前会忽略 `version`、`request_id` 字段，异常或串扰的 FastCGI 响应不会被明确拒绝。
  - 改进：新增记录头解析校验，拒绝非 FastCGI v1 或非当前 request id 的响应，并补充单元测试。

- [x] FastCGI `FCGI_ABORT_REQUEST` 常量未接入。
  - 位置：`crates/core/src/proxy/fastcgi.rs`。
  - 影响：常量目前只靠局部 `#[allow(dead_code)]` 保留，实际取消请求时不会向 FastCGI 后端发送 abort。
  - 改进：删除未接入常量和对应 `dead_code` 豁免，避免暗示已支持取消传播。

- [x] 管理 API 支持 loopback 无 token 访问，文档已提示但默认安全边界依赖部署者。
  - 位置：`crates/core/src/admin/mod.rs`、`crates/core/src/server/mod.rs`。
  - 影响：loopback 监听在容器、端口转发或 sidecar 环境中可能不等同于“只有本机可信用户可访问”。
  - 改进：保留兼容行为，但在 loopback 且未配置 bearer token 时增加启动警告，提示生产和转发/容器化场景配置 token。

- [x] `rustfmt.toml` 包含多项 nightly-only 配置。
  - 位置：`rustfmt.toml`。
  - 影响：stable rustfmt 每次都会输出 warnings，容易掩盖真正格式化问题，也影响 CI 日志可读性。
  - 改进：移除 stable rustfmt 不支持的 nightly-only 配置，保留稳定配置项。

- [x] `benchmarks/http-servers/__pycache__` 被纳入版本控制。
  - 位置：`benchmarks/http-servers/__pycache__/run.cpython-314.pyc`。
  - 影响：生成物进入仓库，增加无意义 diff，也可能造成跨 Python 版本噪声。
  - 改进：删除已跟踪 pyc，并在 `.gitignore` 增加 `__pycache__/` / `*.pyc`。

## 本轮验证

- [x] `cargo check --workspace`
- [x] `cargo test --workspace`
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
