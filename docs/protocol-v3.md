# WinriseF WebTransport Memory Benchmark Protocol v3

状态：浏览器互操作测试协议  
传输：WebTransport over HTTP/3  
端序：所有整数均为 big-endian  
正式前端：`2025-blog-public`（本 Rust 仓库不提供前端）

## 1. 连接角色

- Rust Agent 是 WebTransport server。
- 未安装程序的远端网页是 WebTransport client。
- 不定义 Rust client，也不允许 Agent ↔ Agent。
- 手工 `serve` request path 默认为 `/winrisef/p0`；网页启动流程固定使用 `/winrisef/benchmark/v3`。
- Agent 在接受 session 前精确校验 HTTPS `Origin` 和 path。

## 2. Session 流程

```text
Browser                         Rust Agent
   │── WebTransport CONNECT ───────▶│
   │── open bidirectional control ─▶│
   │── Hello (48 bytes) ───────────▶│
   │◀─ HelloAck (32 bytes) ─────────│
   │                                │
   │  四条单向 data stream          │
   │  方向由 Hello.direction 决定   │
   │                                │
   │◀─ TransferResult (32 bytes) ───│
   │── consume control FIN ─────────▶│
   │◀─ session close ────────────────│
```

第一个应用流必须是 browser 创建的双向 control stream。鉴权成功前，Agent 不接受或创建文件数据流。

一个逻辑测速固定由六个独立 WebTransport/QUIC connection 并行组成；每个 connection 使用一张不同的 120 秒一次性 ticket，并分别执行上面的完整 session 流程。browser 把逻辑 `total_size` 尽量均匀拆给六个 session，完成后以总 bytes 除以六路中最晚完成时间计算聚合吞吐。任一 session 握手或传输失败时，本次逻辑测速失败并关闭其余 connection。v3 不允许退回单 connection 模式伪装成聚合结果。

## 3. Hello

固定长度：48 bytes。

| Offset | Size | Field | Value/meaning |
|---:|---:|---|---|
| 0 | 8 | magic | ASCII `WRNFHEL1` |
| 8 | 2 | version | `3` |
| 10 | 1 | direction | `1` browser→Agent memory；`2` Agent→browser memory |
| 11 | 1 | lanes | 必须为 `4` |
| 12 | 4 | block_size | 必须为 `4194304` |
| 16 | 8 | extent_size | 必须为 `16777216`（memory benchmark 专用 16MiB stripe） |
| 24 | 8 | total_size | `1..=max_transfer_size` |
| 32 | 16 | token | Phase-0 128-bit 临时 token |

`extent_size` 必须是 `block_size` 的整数倍。任何 magic、version、direction 或尺寸错误都会拒绝传输。

## 4. HelloAck

固定长度：32 bytes。

| Offset | Size | Field | Value/meaning |
|---:|---:|---|---|
| 0 | 8 | magic | ASCII `WRNFACK1` |
| 8 | 2 | version | `3` |
| 10 | 1 | status | 见状态码 |
| 11 | 1 | lanes | 回显 lanes |
| 12 | 4 | block_size | 回显 block size |
| 16 | 8 | extent_size | 回显 extent size |
| 24 | 8 | total_size | 回显 total size |

状态码：

| Code | Name |
|---:|---|
| 0 | accepted |
| 1 | authentication_failed |
| 2 | invalid_configuration |
| 3 | busy |
| 4 | transfer_failed |

## 5. Data stream

每次传输固定四条可靠单向流。发送方创建流；browser→Agent 时由 browser 创建，Agent→browser 时由 Agent 创建。

每条流的结构：

```text
LaneHeader
ExtentHeader
payload bytes
ExtentHeader
payload bytes
...
END ExtentHeader
FIN
```

### 5.1 LaneHeader

固定长度：32 bytes。

| Offset | Size | Field | Value/meaning |
|---:|---:|---|---|
| 0 | 8 | magic | ASCII `WRNFLAN1` |
| 8 | 2 | version | `3` |
| 10 | 2 | lane_id | `0..3` |
| 12 | 2 | lane_count | `4` |
| 14 | 2 | reserved | 必须写 `0` |
| 16 | 8 | total_size | 必须等于 Hello |
| 24 | 8 | extent_size | 必须等于 Hello |

四条流的 `lane_id` 必须唯一且完整覆盖 0–3。

### 5.2 ExtentHeader

固定长度：16 bytes。

| Offset | Size | Field | Value/meaning |
|---:|---:|---|---|
| 0 | 8 | offset | 文件/内存源的绝对 byte offset |
| 8 | 8 | length | extent payload 长度 |

普通 extent 必须满足：

- `offset % 16MiB == 0`；
- `length == min(16MiB, total_size - offset)`；
- `offset + length` 不溢出且不超过 `total_size`；
- 同一 offset 只能出现一次；
- 所有 extent 最终必须完整覆盖 `0..total_size`。

结束标记为全零 header：

```text
offset = 0
length = 0
```

END 后不得再出现数据，随后必须 FIN。协议不为每个 4MiB block 增加 header。

## 6. TransferResult

固定长度：32 bytes，由 Agent 在 control send stream 上发送。

| Offset | Size | Field | Value/meaning |
|---:|---:|---|---|
| 0 | 8 | magic | ASCII `WRNFDON1` |
| 8 | 2 | version | `3` |
| 10 | 1 | status | `0` 或 `4` |
| 11 | 1 | reserved | `0` |
| 12 | 8 | bytes | Agent 实际处理 bytes |
| 20 | 8 | elapsed_nanos | Agent 端耗时 |
| 28 | 4 | reserved | `0` |

Agent 只有在四条 lane 全部成功、byte count 一致、extent coverage 完整后才返回 accepted。

每个 Agent session 的吞吐计时从其 HelloAck 写出后开始；browser 聚合计时在六个 HelloAck 全部验证完成后开始，不包含 WebTransport CONNECT、TLS/QUIC 握手或 control hello。64MiB 快速测试均分后每 connection 约 10.7MiB，因此每条 connection 只有一个 payload lane，但六条独立 connection 均参与；1GiB 精准测试会使用全部 24 个 payload lane。每个 Agent session 仍按 `lane_id + n × lane_count` 确定性分配 stripe。

## 7. 内存模式语义

### BrowserToAgentMemory

- browser payload 内容可以是任意字节；
- Agent 使用 Quinn ordered `read_chunk` 取得内部 `Bytes` 后直接计数并丢弃，不复制到应用层临时 payload buffer；
- Agent 不分配 `total_size` 大小的内存；
- 完整性验证针对 stream framing、extent coverage 和 byte count。

### AgentToBrowserMemory

- Agent 只预分配一个共享的 4MiB 零填充 immutable `Bytes`，通过廉价 slice/clone 将所有 payload 所有权直接交给 Quinn；
- browser 应持续读取并丢弃；
- browser 不得累计完整 payload；
- Agent 等待每条 send stream 被对端消费后才报告完成。

## 8. 有界性

- Agent 默认接受六个 active benchmark session，可配置为 1–8；Bridge 使用独立 semaphore，不占测速名额。
- Agent benchmark 的应用层 payload allocation 每 active session 固定为 4MiB，默认六路合计 24MiB，不随逻辑 `total_size` 增长；每条 Quinn connection 内部仍受 128MiB send/receive window 约束。
- browser sender 在六路之间共享四个 4MiB source block，每个 session 的每 lane 最多两个 `writer.write` 同时在途；不得无界排队 Promise。
- 默认每 session 四个 lane task，六路合计最多 24 个 data lane task。
- 不创建 per-block task、thread、channel、Rust heap copy 或逐块日志。
- Phase 0 不包含断点续传、多文件并发、逐块 hash 或逐块 ACK。

连接建立时 browser 请求 `congestionControl: "throughput"`、要求 unreliable/HTTP3 transport，并预告四条 incoming unidirectional stream。Agent 使用 BBR、1MiB initial congestion window、10ms LAN initial RTT 和同优先级 stream round-robin。每次 payload 结束只输出一条 QUIC summary，记录 RTT、cwnd、丢包、MTU、UDP batching 与流控 blocked frame；不得用 packet trace 代替该低频摘要。

## 9. 启动 Bridge 与一次性 ticket

Windows 开发版可把当前二进制注册为 `winrisef://` handler。网页使用：

```text
winrisef://launch?returnUrl=<same-origin callback page>&nonce=<128-bit nonce>
```

Agent 只接受内置正式 Origin、本机 loopback Origin，或注册 handler 时通过 `--trusted-origin` 明确写入的精确 HTTPS Origin；不会信任任意网页提供的 `returnUrl`。

Agent 只接受 HTTPS return URL，开发环境额外允许 loopback HTTP。回调 fragment 包含 loopback Bridge endpoint、局域网 benchmark endpoint、短期 P-256 证书 SHA-256、一次性 launch token、nonce 和过期时间；query string、日志和 Supabase 中不包含 token。

安装端网页连接 `/winrisef/bridge/v1` 后在首个双向流发送 32-byte `WRNFBH01` hello。launch token 验证成功后只能消费一次。Bridge 使用固定 16-byte `WRNFTR01` request 和 40-byte `WRNFTS01` response 签发 120 秒有效、只能消费一次的 benchmark ticket。一个 v3 逻辑测速必须独立申请六张 ticket；不得把单张 ticket 改成六次可消费。安装端网页通过现有加密 WebRTC DataChannel 把 ticket 交给指定纯网页连接，文件/测速 payload 不经过安装端网页。

如果双方都发布本机 Agent capability，网页按稳定 device ID 排序，只保留一端 Agent；不会建立 Agent ↔ Agent。

## 10. 与后续正式协议的关系

当前 memory ticket 只授权一次内存测速。正式文件数据面仍会加入：

- attachment manifest；
- 系统文件选择与 positional I/O；
- `.part` 完成策略。

这些扩展不得改变“一次会话最多一个 Agent”和“Rust 仓库无前端”两项不变量，也不得在文件 payload 热路径加入 JSON。
