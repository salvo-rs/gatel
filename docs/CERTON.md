# certon 0.3 集成边界

本项目升级至 `certon 0.3.0`，保留 `default-features = false` 与 `ring`。
以下决策根据该版本发布源码及本地集成测试，不将库具备的功能等同于 Gatel 已开放的功能。

2026-09-09 复核：crates.io API 的 `max_stable_version` 与 `newest_version`
均为 `0.3.0`；工作区清单和锁文件一致。Windows 上重新执行
`cargo test -p gatel-tls --all-targets --locked --quiet`，88 项测试通过；
`cargo clippy -p gatel-tls --all-targets --locked -- -D warnings` 通过。
该次验证不是全工作区、真实外部 CA 或其他平台的验收。

## 哪些统一，哪些分层

| 概念 | 当前处理 | 原因 |
|---|---|---|
| `CertStore` / `CertificateResource` | 直接采用 certon 契约，生产签发、缓存恢复使用 `AtomicCertStore` | 不再自创证书持久化接口；证书、私钥、issuer 元数据完整往返 |
| 旧 `Config` → `Policy` / `CertManager` | 暂不取代 Gatel 的声明配置和运行管理器 | `Policy` 是签发策略；Gatel 配置还决定资源身份、命名空间和 generation 发布。固定提前续期时长也不等同于有效期比例 |
| 账户 `Storage` | 保留受限适配器 | 0.3 的账户保存协议仍需要本地账户成对原子记录、注册 intent 和 OS 文件锁；证书 `CertStore` 不包含账户 |
| 活动 `CertificateStore` 与 certon `CertCache` | 保留 Gatel 运行时 store | ResourceId、失效签发令牌、reload 原子发布与域名缓存不是同一契约 |

`AtomicCertStore` 已公开并实现 `certon::CertStore`，测试也将它注入真正的
`certon::CertManager` 后加载证书。生产续期仍由 Gatel 管理生命周期和发布权限，
不是同时启动两个自动续期管理器。当前没有配置化远程 store 工厂。

## 持久化与迁移

- 每个后端限定一个账户命名空间和 issuer；写入者仍须由宿主协调。
- 新证书写为 `certificate-<hash>.identity`：有版本的 JSON 单记录，包含资源元数据及显式 PEM 字段。不能直接序列化 `CertificateResource` 代替，因为它跳过 PEM 字段。
- 完整记录不超过 256 KiB。写入前验证证书/私钥和声明名称覆盖，使用同目录私有临时文件、sync、rename；Unix 同步父目录。Windows 权限继承目录 ACL，不承诺任意文件系统断电持久性。
- 新记录缺失时，可只读加载同一命名空间的旧 `<host>.identity`，再回退 `.crt`/`.key`。新记录损坏明确返回错误，不回退旧材料。更早基础目录的无账户归属缓存不自动导入。
- 删除写入 tombstone，阻止旧缓存复活；保留旧文件用于人工恢复，因此不是私钥安全擦除。删除不存在的目录中的资源成功且不创建目录。
- 名称键遵循该版本 `names_key()` 实现，但拒绝排序后逗号拼接超过 1024 字节的 SAN 集合：该版本会截断而非散列完整集合，不同集合可能形成同一个键。写入和读取均拒绝这种有损键；恰好 1024 字节仍可正常保存。当前支持具体 DNS/IP 名称，不开放通配符管理。名称校验使用 rustls SAN 校验；certon 的 `names` 还包含 CN，不能与 SAN 列表直接比较。
- trait 是异步接口，但本地后端仍执行有界同步文件 I/O；没有宣称非阻塞文件后端、跨主机一致性或恶意父目录防护。

## 生命周期与后续方向

证书发布暂存中被替换、移除或因并发续期而未采用的材料，在宿主的
`finish_publication` 阶段释放，不再由当前 runtime 持有到下一次退休。
该阶段在 generation 锁及所有发布 guard 释放后执行；现有 TLS 读者仍通过独立
Arc 保有自己的 certified key。释放多余引用不等于保证底层密码实现或分配器完成安全擦除。
后续 generation 可以在旧 finish 尚未结束时提交；清理只取走本次发布记录中的退休材料，
不能根据当前证书表重新挑选删除对象。状态与证书的逆序 finish 已有弱引用回归测试。

ACME worker 只持有独立续期状态，不反向持有管理器，避免管理器与 worker 的强引用环。
最后一个管理器释放会取消 worker；确定性等待和存储排空仍需显式 `shutdown()`。

后续开放 ARI、OCSP、DNS-01 或受控 On-Demand 时，再选择对应 `Policy` 字段作为
规范化值，逐项验证配置语义。若采用完整 `CertManager` 执行签发，必须先接入
Gatel 的取消、账户协调、候选证书验证和 generation 发布边界，不能直接让库缓存
绕过宿主发布。0.3 的 RAII storage lock 也不替代这些职责。

当前测试使用本地协议 CA 与自签 fixture；真实外部 CA staging、macOS、断电恢复
和远端 CI 仍需单独验收。
