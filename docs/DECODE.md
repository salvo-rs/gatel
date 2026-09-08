# 可选请求解压

使用 `cargo build -p gatel --features decode` 启用独立 `gatel-decode` 插件。
默认二进制不链接该插件，不声明 `decode` 能力；包含该指令的配置会被拒绝。
示例见 `plugins/decode/examples/gatel.kdl`。

```kdl
route "/upload" {
    body-limit "8MiB"
    decode max-encoded="8MiB" max-decoded="8MiB" max-ratio=100 max-inflight=8 timeout-ms=10000
    proxy upstream="application"
}
```

## 执行与限制

- 支持 gzip、br、zstd、HTTP deflate（zlib 封装）。无 Content-Encoding 或 identity 原样继续；其他编码、多层编码、重复 Content-Encoding 返回 415，不静默透传。
- 按流读取压缩输入，分块解压到有界缓冲。完整检查成功后才转交下游；不是边解压边向后端发送，不适合无界上传或无限事件流。这样格式错误、校验失败和超限不会让后端先收到部分请求。
- 默认压缩/解压上限分别 8 MiB、倍率 100、并发 8、读取与解压时限 10 秒。可配置字节上限 1～64 MiB、倍率 1～1000、并发 1～64、超时 1～300000 毫秒。
- 膨胀倍率按完整解压大小 / 已读取压缩大小检查，绝对输出上限始终逐块检查。Zstd 窗口单独限制为 8 MiB；Brotli 等解码器也有内部工作内存，不能把 max-decoded 当成精确 RSS 限额。输入切为最多 8 KiB 的片段，并在输入、输出两端主动让出执行权，覆盖没有输出的连续空成员。timeout 仍不能中断单次同步解码调用。
- 超限返回 413，格式/校验/截断/尾部垃圾或不支持的 trailer 返回 400，超时返回 408，准入耗尽返回 503。gzip 连续成员会全部处理，不只取首个。空的编码字节流视为错误。
- Content-Range 或请求 Cache-Control: no-transform 与解压冲突，返回 415。成功后更新 Content-Length，去掉 Content-Encoding、旧 framing/trailer 与内容摘要头；不验证原请求摘要或签名。

## 与宿主和插件组合

`TransformRequestBody` 是 SDK 的类型化执行效果：宿主/guard 安装 body 上限后才能解压，
解压先于 `ReadRequestBody` / `CallUpstream` 消费者。用户 `order` 不能绕过这些约束。
同一路由最终链上的两个 decode 会在编译时冲突，不尝试猜测多层变换顺序。

SDK `RequestBodyLimit` 将已安装的最小宿主/guard 上限传递给变换器，decode 同时将它用于
解压后的字节；不能通过换成新 body 绕过 body-limit。此元数据是可信插件合作契约，
不是进程沙箱。未知长度的压缩输入仍由原有 streaming guard 计数。

准入按指令资源身份保存到宿主 StateRegistry，相同身份且并发上限相同的 reload 复用
Semaphore。调整解压大小、倍率和 timeout 不清空活动许可；改变身份或并发容量会创建
新的状态，旧代仍可排空，因此不承诺跨身份/不兼容 reload 的全局总容量。
解压后 `Bytes` 的所有副本释放前一直持有许可，而不是 handler 返回就释放。
取消、超时和格式失败会释放此次许可；无额外 detached worker 或等待队列。

当前测试覆盖四种编码的 HTTP/1.1 定长/chunked、HTTP/2 到真实 HTTP 后端、拒绝不转发、
多种上限、取消、超时、字节副本生命周期、兼容 reload 和 feature 裁剪。
gzip/zlib 另有 Python 独立生成的二进制固定样本与损坏校验和测试；还覆盖逐字节分帧、
完整成员后的输入错误/trailer、空成员取消，以及 Zstd 首个/后续成员的大窗口拒绝。
`request_decode` fuzz 入口与稳定测试共用不变量，已执行一次默认 sanitizer 短测；
范围与数据见 [fuzz README](../fuzz/README.md)。独立网络客户端/浏览器、长稳、持续
fuzz 和大规模资源压力仍需后续验证。

旧 main 的请求解压实现只作行为核对来源：本版本有意改变未知编码透传、失败继续处理、
默认 64 MiB 和 raw deflate 等行为；不能宣称旧配置完全兼容。
