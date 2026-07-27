# WinriseF 单端原生增强传输开发计划

状态：架构修订版 2（LNA HTTP/TCP 主数据面）

日期：2026-07-18

Rust 基线：1.97.1

当前发布版本：`v0.1.0-beta.1`（Windows 资源版本 `0.1.0.1`）

Rust 仓库：`E:\Project\PROJECT\2026-Rust_Native_Transfer`

网页仓库：`E:\Project\PROJECT\2025-blog-public`

## 1. 本次架构纠正

此前“双端都安装 Rust 客户端，再使用原生 QUIC/TCP 互传”的方案永久作废。正式产品在任何一次会话中最多且永远只有一端安装 Rust Agent，另一端只使用现有网页。即使两端都是桌面电脑，也只能选择其中一端启用 Agent，不能形成 Agent ↔ Agent 连接。

正式拓扑是：

```text
安装端设备 A
┌────────────────────────────────────────────┐
│ 网页 A  ⇄  本机 Rust Agent                │
│  UI/会话      文件选择、原生文件 I/O       │
└──────────────────┬─────────────────────────┘
                   │ 首选：Chrome LNA + HTTP/1.1/TCP 多 XHR
                   │ 兼容：WebTransport / HTTP/3 / QUIC 六路
                   │ 远端网页主动连接
┌──────────────────▼─────────────────────────┐
│ 纯网页设备 B                               │
│ 浏览器 XHR/Blob + File/Storage API          │
│ 不安装程序                                 │
└────────────────────────────────────────────┘
```

必须始终成立的产品承诺：

- 一次会话只允许其中一台设备安装和启用 Rust Agent，这不是最低要求而是产品上限。
- 即使两端都是 Windows/Linux/macOS 电脑，也不得同时启用两个 Agent。
- 未安装 Agent 的设备仍能通过现有网站加入会话并收发文件。
- 两端都是手机时不使用 Agent 和 WebTransport 加速，完全沿用网页 ↔ 网页的现有 WebRTC 传输。
- Rust 项目不提供网页、桌面窗口或其他前端；所有产品 UI 都在 `2025-blog-public`。
- Windows Agent 只以单个便携 EXE 发布：无安装器、无服务、无托盘、无开机启动；首次双击仅在当前用户范围自注册 `winrisef://` 并打开官网结果页。
- EXE 直接在用户选择的位置运行，不复制自身；移动或重命名后再次双击，用新路径覆盖协议注册。
- 浏览器不能监听端口；纯网页端始终是连接发起方，Rust Agent 同时提供受限 HTTP/TCP 数据 API 和 WebTransport 兼容端点，不提供网页或 UI。
- Chrome 142+ 的 Local Network Access（LNA）权限允许正式 HTTPS 页面在用户授权后请求局域网 Agent 的明文 HTTP endpoint；该路径是极速模式默认数据面。
- 用户明确拒绝 LNA 权限时，本次会话的极速模式不可用，直接保留现有 WebRTC DataChannel，不得改试 WebTransport 来绕过用户决定。
- 只有浏览器根本不识别 LNA permission descriptor 时，才回退到已验证的上一版六路上传、六路下载 WebTransport/QUIC；支持 LNA 但端点不可达、CORS 错误或 Agent 版本过旧均应明确报错，不伪装成“不支持”。

## 2. 项目目标

### 2.1 核心目标

- 在不要求远端安装程序的前提下，让安装端的大文件读写绕过浏览器内存和文件 I/O 热路径。
- 安装端发送时由 Rust 直接读取文件并向远端网页发送，文件字节不经过网页 A。
- 安装端接收时由 Rust 直接将远端网页的数据写入目标文件，文件字节不经过网页 A。
- 复用现有 `/t` 局域网会话、聊天 UI、Supabase Realtime 配对和 WebRTC 控制能力。
- LNA HTTP/TCP 是首选高速附件数据面，六路 WebTransport 是浏览器不支持 LNA 时的兼容数据面；现有 WebRTC 继续承担聊天、能力协商、增强通道引导和完整回退。
- 传输 50GB 文件时，Rust Agent 与浏览器内存都保持有界，不随文件大小线性增长。
- 极致单文件吞吐是首要目标；断点续传、多文件公平调度、跨会话恢复和历史等高级能力只能在不降低热路径吞吐、不增加明显内存压力和不扩大关键状态机的前提下加入。

### 2.2 性能目标

- 先用 `iperf3` 测量同方向网络上限。
- LNA HTTP/TCP 内存源/汇在目标局域网达到同方向 OpenSpeedTest/`iperf3` 单向基线的 90% 以上，或证明已经达到浏览器/网卡的稳定上限。
- 安装端 NVMe 文件到纯网页端可直接落盘时，完整文件吞吐达到内存传输结果的 90% 以上。
- 纯网页端发送到安装端 NVMe 时，完整文件吞吐达到内存传输结果的 90% 以上。
- LNA 主路径默认使用六个并发 XHR worker；每个 worker 以约 30MiB 的有界 HTTP 请求循环复用浏览器 TCP connection，逻辑总量不一次性进入手机内存。
- WebTransport 兼容路径固定恢复到实测上一版的六路上传、六路下载；每条 connection 一个双向控制流和四条单向数据流。
- Rust 默认使用 64MiB extent、4MiB I/O block、每 lane 两个复用 buffer。
- WebTransport 兼容性能门使用协议 v3：两个方向都把逻辑总量均分到六条独立 connection；每条内部使用 16MiB stripe 和四条 lane，正式大文件数据面仍使用 64MiB extent。
- 50GB 文件传输期间 Rust Agent 常驻内存目标低于 128MiB；网页端额外传输内存目标低于 128MiB。

### 2.3 非目标

第一版不实现：

- 两个 Rust Agent 之间的正式原生 QUIC/TCP 协议；
- 要求双方安装客户端；
- 远端浏览器监听端口；
- NAT 端口映射、互联网直连或自建 TURN；
- 数据库、传输历史云同步或服务端文件中转；
- 安装端独立 GUI、托盘、自动更新；
- Rust 仓库内的网页、benchmark 前端或桌面前端；
- 跨刷新持久断点续传；
- 多个大文件并行占满多条独立 QUIC connection；
- 压缩、逐块哈希、应用层逐块 ACK；
- 在没有 LNA 权限的浏览器中用任意方式绕过或诱导授权。

## 3. 现有网页能力与复用原则

网页仓库已经包含 LAN Session V11：

- `src/lib/lan-transfer/signal-client.ts`：Supabase Presence/Broadcast 信令；
- `src/lib/lan-transfer/native-webrtc-transport.ts`：可靠有序 DataChannel；
- `src/lib/lan-transfer/reconnect-coordinator.ts`：连接恢复状态机；
- `src/lib/lan-transfer/connection-runtime.ts`：聊天、附件协议、进度、取消和同页恢复；
- `src/lib/lan-transfer/attachment-send-scheduler.ts`：有界附件调度；
- `src/lib/lan-transfer/storage/`：Memory、OPFS、IndexedDB 和直接文件写入；
- `src/app/toolbox/use-lan-transfer-controller.ts`：页面会话和 Transport 装配入口。

新方案不复制这些业务能力。增强路径只增加端口和适配器：

```text
现有会话/聊天/附件业务层
        │
        ├─ WebRTC Transport（所有浏览器可用，控制与回退）
        ├─ Native Agent Control Adapter（安装端网页 ⇄ localhost Agent）
        ├─ LNA HTTP Bulk Adapter（纯网页 ⇄ Agent，默认大文件数据面）
        └─ WebTransport Bulk Adapter（仅 LNA 不受支持时兼容）
```

WebRTC 在增强模式中仍然有价值：

- 完成原有二维码配对和设备身份建立；
- 交换 Agent capability、局域网 endpoint、证书 SHA-256 和一次性 session ticket；
- 传输聊天、语音、小图片和低频附件控制；
- LNA 被拒绝或增强数据面失败时回到现有 WebRTC 附件路径；只有 LNA API 不受支持才选择 WebTransport 兼容路径。

文件 payload 一旦进入增强模式，就不得绕回 WebRTC 或网页 A。

## 4. 两类连接

### 4.1 本机控制连接

```text
网页 A ── WebTransport(loopback) ── Rust Agent
```

职责：

- Agent 发现与版本协商；
- 系统文件选择器和保存位置选择；
- Windows 下所有系统文件对话框使用同一个短生命周期 owner 适配层获取前台归属；owner 不显示在任务栏，并在确认、取消或失败后立即销毁；
- 创建远端连接邀请；
- 将低频控制事件、进度、完成和错误推送给网页 A；
- 取消传输和关闭会话。

启动方式：

1. 网页 A 的用户点击“启动原生增强”。
2. 网页调用 `winrisef://launch?...` 自定义协议启动 Agent。
3. Agent 先取得单实例互斥、绑定同端口 UDP/TCP，并生成短期启动凭据。
4. Agent 立即打开或回调网站 HTTPS 页面；回调信息只放在 URL fragment，包含 loopback 端口、证书 SHA-256、一次性 launch token 和过期时间。
5. 网页 A 使用 `serverCertificateHashes` 建立 loopback WebTransport，Agent 校验 Origin、launch token、有效期和一次性使用状态。
6. 如果检测到 GUA 公网 IPv6，Agent 在 Bridge 可连接后异步检查或申请 Windows 防火墙规则；该 UAC 不阻塞本机连接。
7. 私有 IPv4、CGNAT 和 ULA endpoint 可立即发布；GUA IPv6 只在防火墙状态为 `available` 后通过 Bridge V3 endpoint snapshot 发布。拒绝或失败只关闭公网 IPv6 路径，不关闭 Agent、LNA HTTP 或 WebRTC。

不使用固定的“跳过 TLS 校验”，也不要求用户手动信任自签名证书。

### 4.2 远端数据连接

```text
纯网页 B ── LNA HTTP/1.1/TCP 多 XHR（默认） ── Rust Agent A
          └─ WebTransport/QUIC 六路（仅不支持 LNA）
```

职责：

- 远端网页 B 始终主动连接；
- Agent 在同一数字端口监听 TCP/HTTP1 和 UDP/HTTP3；HTTP endpoint 只是数据 API，不是 Rust 前端。
- HTTP 路径按精确 Origin、方法、路径、Content-Length 和一次性 peer ticket 鉴权，并提供严格 CORS/LNA 响应头。
- 网页使用六个并发 XHR worker 发送/接收约 30MiB 的有界请求；Agent 流式丢弃测速上传或流式生成测速下载，不分配逻辑总量。
- WebTransport 兼容路径保留既有 control stream 和每 connection 四条可靠单向数据流，双方向均为六个 connection。

Agent endpoint 引导流程：

1. 网页 A 和网页 B 先通过现有 WebRTC 建立会话。
2. 网页 A 从本机 Agent 获取 LAN HTTP endpoint、WebTransport endpoint、证书 hash、一次性 peer ticket 和过期时间。
3. 网页 A 通过已加密的 WebRTC DataChannel 将 Native Capability 发给网页 B。
4. 网页 B 在用户动作下查询 `local-network-access` permission：`denied` 关闭极速模式；permission 名称不受支持才选择六路 WebTransport；`prompt/granted` 先请求 HTTP probe 并处理 LNA 提示。
5. LNA 成功时，网页 B 通过自定义 header 为每个 HTTP 数据请求提交一张一次性 ticket；WebTransport 兼容时仍在第一条控制流提交 ticket。
6. Agent 验证 Origin、请求边界和 ticket 成功后才读取或发送 payload。

第一版不通过公开 Supabase payload 直接发送可用 ticket；以后若必须脱离 WebRTC 引导，再单独设计端到端加密的信令扩展。

## 5. 数据流

### 5.1 安装端向纯网页端发送

```text
网页 A 发起选择文件
  → Agent 打开系统文件选择器
  → Agent 返回文件元数据，不返回文件字节
  → 网页 A 通过现有会话发送 attachment offer
  → 网页 B 选择浏览器存储并接受
  → Agent positional read
  → 4 条 WebTransport 单向流
  → 网页 B File System Access / OPFS / IndexedDB / Memory
  → 网页 B 最终确认
  → Agent 通知网页 A 完成
```

### 5.2 纯网页端向安装端发送

```text
网页 B 选择 File
  → attachment offer 到网页 A
  → 网页 A 请求 Agent 打开系统保存对话框
  → Agent 预分配 .part 文件并接受
  → 网页 B 用 4MiB read cache 读取 File
  → 4 条 WebTransport 单向流
  → Agent positional write
  → 长度/extent 完整性检查
  → sync + .part 原子改名
  → Agent 发送最终确认并通知网页 A
```

取消或失败时保留 `.part` 仅限未来恢复阶段；第一版默认删除未完成临时文件，且必须明确记录这一产品行为。

## 6. 协议原则

### 6.1 版本

- Native Agent bridge protocol 与 bulk transfer protocol 分别具有独立 major version。
- major 不一致直接拒绝并提示刷新网页或升级 Agent，不维护复杂兼容分支。
- 现有 LAN Session V10 的升级由网页仓库单独决定；不要把 Rust crate 版本当作网页协议版本。

### 6.2 控制面

低频控制消息允许使用严格 schema 的 JSON，便于 Rust 与 TypeScript 调试和演进。每条消息必须包含：

- `protocolVersion`；
- `id` 或 `requestId`；
- `sessionId`；
- `type`；
- 与 peer/attachment 相关时包含对应稳定 ID。

控制消息需要限制最大长度、拒绝未知 major、拒绝越权 attachment ID。JSON 不得出现在文件块热路径。

### 6.3 数据面

文件数据使用固定二进制 header：

- stream header：magic、协议版本、session ID 摘要、attachment ID、lane ID、lane count、总大小；
- extent header：offset、length、flags；
- payload：原始文件字节；
- lane 结束使用 stream FIN，不为每个 block 发送 JSON 或 ACK。

默认调度：

```text
laneCount  = 4
extentSize = 64 MiB
ioBlock    = 4 MiB
pool       = 每 lane 2 buffers
```

每个发送 lane 使用原子 `fetch_add(64MiB)` 领取 extent，在 extent 内连续读写 4MiB block。接收端按 offset 写入，因此不同 lane 的到达顺序不影响文件布局。

第一版一次只允许一个增强大文件处于 active data 状态；其他附件留在现有调度队列。完成单文件正确性和吞吐后再加入多文件公平调度。

## 7. 安全模型

### 7.1 TLS 与证书

- WebTransport 只能运行在安全上下文，TLS 校验失败不可通过浏览器警告页绕过。
- Agent 使用 P-256 短期自签名证书，证书有效期不得超过 WebTransport certificate-hash 机制允许的两周；项目默认每次 Agent 启动生成并使用不超过 13 天的证书。
- 网页通过 `serverCertificateHashes` 校验精确的 SHA-256 DER 证书摘要。
- 本机 hash 通过自定义协议启动回调的 URL fragment 交付；远端 hash 通过已经建立的 WebRTC 加密通道交付。
- 不在 localStorage 长期保存私钥、launch token 或 peer ticket。

### 7.2 Agent 鉴权

- Agent 必须校验 HTTPS 页面的精确 Origin allowlist。
- launch token 和 peer ticket 至少 128 bit 随机、短期、一次性使用。
- peer ticket 必须绑定 session ID、目标 device ID、允许的操作和过期时间。
- 未鉴权 session 只能提交 hello，不能打开数据流或访问文件。
- Agent 不接受网页传入任意文件系统路径；文件由系统选择器返回句柄，或由 Agent 内部生成受控临时路径。
- 日志禁止记录完整 token、证书私钥、文件内容、完整局域网地址或用户绝对路径。

### 7.3 网络边界

- WebTransport 没有 ICE/STUN NAT 穿透能力，本阶段只承诺同一局域网可达地址。
- UDP、系统防火墙、不同地址族或浏览器 Local Network Access 权限都可能阻断增强通道。
- 所有失败必须分类为“浏览器不支持、权限拒绝、Agent 不可达、TLS/hash、鉴权、协议、文件 I/O、超时”，随后允许 WebRTC 回退。

## 8. Rust 工程结构

采用精简的 Clean/Hexagonal 边界，避免让业务核心依赖 Tokio、WebTransport 或平台 API：

```text
2026-Rust_Native_Transfer/
├─ Cargo.toml
├─ rust-toolchain.toml
├─ plan.md
├─ agent.md
├─ crates/
│  ├─ winrisef-core/
│  │  └─ src/
│  │     ├─ domain/          # session、attachment、状态和值对象
│  │     ├─ protocol/        # 控制 schema、二进制 header、版本
│  │     ├─ scheduler/       # extent、lane、buffer 规则
│  │     └─ ports/           # transport、file source/sink、events、clock
│  └─ winrisef-agent/
│     └─ src/
│        ├─ application/     # 用例编排，不直接解析 socket 字节
│        ├─ adapters/
│        │  ├─ webtransport/ # loopback 与 LAN session
│        │  ├─ file_io/      # 平台 positional I/O 常驻线程
│        │  ├─ file_picker/  # 系统文件选择器
│        │  ├─ launch/       # 自定义协议与浏览器回调
│        │  └─ telemetry/    # 本地每秒性能统计
│        ├─ config.rs
│        ├─ bootstrap.rs
│        └─ main.rs
└─ tests/                    # 协议、调度、假适配器与互操作测试
```

依赖方向：

```text
winrisef-agent adapters → application → winrisef-core
```

`winrisef-core` 禁止依赖 `tokio`、`quinn`、WebTransport crate、`rfd`、`sysinfo` 或操作系统模块。核心用例通过 ports 接口调用外部能力，单元测试使用 memory/fake adapters。

## 9. 网页工程结构调整

在不破坏现有 WebRTC 代码的前提下，计划新增：

```text
2025-blog-public/src/lib/lan-transfer/
├─ native-agent/
│  ├─ capability.ts         # feature detection 与 Agent 状态
│  ├─ launch-client.ts      # 自定义协议启动与 fragment 消费
│  ├─ local-bridge.ts       # 网页 A ⇄ Agent
│  ├─ peer-webtransport.ts  # 网页 B ⇄ Agent
│  ├─ protocol.ts           # 控制 schema 与二进制 header
│  └─ bulk-transfer.ts      # 浏览器端流、backpressure 和 storage 接入
└─ ...现有 V10 模块
```

需要扩展而不是复制的现有边界：

- `LanCapability`：加入可选 WebTransport 和 Native Agent capability；
- `LanConnectionRuntime`：允许附件选择 `webrtc` 或 `native-webtransport` 数据面；
- `LanAttachmentSendScheduler`：增强文件交给 bulk adapter 后不再写 DataChannel frame；
- `LanStorageEngine`：继续作为纯网页 B 的接收存储端口；
- `use-lan-transfer-controller.ts`：装配 Agent 状态，但不承载协议细节；
- `ARCHITECTURE.md`：实现网页改动时同步更新正式说明。

不要直接把 WebTransport 分支堆入 `native-webrtc-transport.ts`。

## 10. 推荐依赖方向

最终版本以当时锁文件和编译结果为准，第一轮候选：

- Runtime：`tokio`；
- WebTransport/HTTP3：优先评估 `web-transport-quinn`，封装在 adapter 后；
- QUIC/TLS 底层：`quinn`、`rustls`；
- 短期证书：`rcgen`；
- 固定 buffer：`bytes` 或自有 buffer pool；
- socket 调优：`socket2`；
- 错误：`thiserror`，仅进程边界可使用 `anyhow`；
- 控制消息：`serde`、`serde_json`，只用于低频控制面；
- 随机和摘要：`rand`、`sha2`、必要时 `hkdf`/`hmac`；
- 文件选择：`rfd`；
- 本地观测：`tracing`、`sysinfo`；
- 文件预分配：标准库能力或经过跨平台验证的轻量 crate。

不在首版加入数据库、ORM、Web 框架全家桶、嵌入式浏览器、自动更新框架或远程遥测 SDK。

## 11. 实施阶段

### Phase 0（第一阶段）：无前端原生高速内核

目标：先完成可供现有网页连接的无前端 Rust 高速内核。该阶段不在 Rust 仓库创建网页客户端，也不实现 Agent ↔ Agent；浏览器互操作和实测在接入现有网页后完成。

任务：

1. 审核并删除/重写当前由错误双原生方案产生的临时 scaffold；不得补完双 Rust CLI。
2. 按第 8 节建立 `winrisef-core` 与 `winrisef-agent`。
3. Agent 创建短期证书并启动浏览器可连接的 WebTransport server，同时在同数字端口提供受限 LNA HTTP/TCP API。
4. 实现受限的 session hello 和 benchmark-only 鉴权入口，为现有网页 adapter 固定协议契约。
5. 完成 memory source/sink 的 Agent 侧双向引擎；测试大小只作为计数，不分配等量内存。
6. LNA 默认路径实现六个并发 XHR worker 和 30MiB 有界请求循环；WebTransport 兼容路径在两个方向都使用六条独立 connection，每条一个控制流和四条单向数据流，任一路失败即关闭整组。
7. 实现 memory benchmark 的 16MiB 固定 lane stripe、Rust 双向零拷贝 payload 和有界 browser writer backpressure；4MiB buffer pool 保留给后续真实文件 I/O。
8. 实现固定二进制 stream/extent header，热路径无 JSON。
9. 输出 elapsed、bytes、current/average Mbps、CPU、RSS 和错误分类。
10. 提供协议 fixture 和 core 假适配器测试，不提供网页或桌面 UI。

完成门槛：

- Rust 侧双向内存传输状态机和协议 fixture 正确；
- 传输大小与计数完全一致；
- 无无限队列，内存不随测试大小增长；
- `cargo check`、Clippy 和测试构建验证通过；
- 不启动 Agent 或 benchmark 完成静态交付；浏览器互操作与 `iperf3` 性能门在 Phase 2 接入现有网页后执行。

### Phase 1：本机 Agent 启动与安全 Bridge

目标：网页 A 可以安全发现并控制本机 Agent，不承载文件 payload。

任务：

1. 实现便携 EXE 无参数双击自注册 `winrisef://`，成功或失败后打开官网结果页；
2. 实现启动回调 fragment；
3. 实现一次性 launch token；
4. 实现 loopback WebTransport bridge；
5. 实现 Origin allowlist、版本握手和 capability；
6. 实现 `select_files`、`select_destination`、`create_peer_ticket`、`cancel_transfer`；
7. 推送 Agent 状态、进度、完成和错误事件；
8. Agent 无独立 GUI，只允许系统文件对话框。

完成门槛：网页不能凭猜测端口控制 Agent；token 重放、错误 Origin、过期 token 和版本不一致全部被拒绝。

### Phase 2：远端网页连接 Agent

目标：网页 B 不安装程序即可安全建立 Agent 数据连接。

任务：

1. 扩展网页 capability，检测 Chrome LNA permission，并保留 WebTransport capability；
2. 网页 A 通过现有 WebRTC 发送 Native Agent offer；
3. 网页 B 默认使用 LAN HTTP endpoint 和每请求 peer ticket；LNA permission descriptor 不受支持时才使用 WebTransport endpoint、hash 和每 connection ticket；
4. Agent 校验 HTTP/WebTransport 的精确 Origin、请求边界与一次性 ticket；
5. UI 显示“原生增强连接中/已启用/已回退”；
6. 区分 Local Network Access `denied`、API 不受支持、TCP endpoint 失败、UDP/防火墙和地址族错误；
7. 失败保持现有 WebRTC 会话，不销毁聊天 Runtime。
8. 在现有网页中加入仅开发环境可见的内存源/汇性能入口，不在 Rust 仓库创建前端。

完成门槛：增强连接失败不会影响文字聊天或普通 WebRTC 文件传输；目标设备连续五轮 LNA HTTP/TCP 内存测试中位数达到同方向 OpenSpeedTest/`iperf3` 的 90%，相对中位数波动不超过 5%。

### Phase 3：安装端发送大文件

目标：Rust 直接从磁盘向网页 B 发送。

任务：

1. 系统文件选择器只返回元数据到网页 A；
2. 文件由常驻专用 I/O 线程打开一次并按 offset 读取；
3. 四 lane 领取 64MiB extents；
4. 每 lane 使用两个 4MiB pooled buffers；
5. 网页 B 接入现有 StorageEngine；
6. 最终验证 total bytes、extent coverage 和文件长度；
7. 完成、拒绝、取消和失败映射到现有附件卡状态。

完成门槛：文件 payload 不出现在网页 A 的 JS heap；50GB 文件 Rust 内存保持有界。

### Phase 4：安装端接收大文件

目标：网页 B 直接向 Rust 发送并由 Rust 落盘。

任务：

1. 网页 B 以 4MiB read cache 读取 File；
2. Rust 打开系统保存对话框；
3. 目标 `.part` 文件预分配；
4. 四个常驻 writer 按 offset 写入；
5. 完成后 flush/sync、长度检查和原子改名；
6. 失败与取消遵循明确的 `.part` 清理策略；
7. 最终确认后更新双方附件状态。

完成门槛：网页 A 不接触文件 payload，目标文件字节与源文件一致。

### Phase 5：可靠性、回退和网页正式集成

目标：增强路径成为现有 LAN Session 的可选数据面，而不是第二套产品。

任务：

1. 将已经在 `2025-blog-public` 开发入口验证过的 adapter 接入正式 LAN UI；
2. 保留一个聊天 Runtime 和一份附件状态；
3. 传输开始前可回退 WebRTC；已开始的增强传输失败时第一版明确失败并允许用户重试；
4. 后续再加入 extent checkpoint 和同页恢复；
5. 网页 feature flag 分阶段启用；
6. 更新网页仓库 `ARCHITECTURE.md`；
7. 删除不再使用的实验代码，不保留双协议兼容泥层。

### Phase 6：跨平台、打包与发布

任务：

1. Windows 单文件便携发布、自注册协议、防火墙与删除前撤销注册说明；
2. Linux deb/AppImage 与协议注册；
3. macOS 签名、公证、文件与网络权限；
4. Rust 构建矩阵和网页协议兼容矩阵；
5. Agent 版本过旧提示；
6. 下载页和原生增强开关；
7. 默认仍允许纯网页 WebRTC 模式。

## 12. 验证矩阵

每个方向分别测试：

| 编号 | 发送端 | 接收端 | 目的 |
|---|---|---|---|
| B0 | Agent memory | Browser discard | Agent → Browser 网络上限 |
| B1 | Browser memory | Agent discard | Browser → Agent 网络上限 |
| B2 | Agent file | Browser discard | 安装端读盘影响 |
| B3 | Agent memory | Browser storage | 浏览器写盘影响 |
| B4 | Browser File | Agent discard | 浏览器读盘影响 |
| B5 | Browser memory | Agent file | 安装端写盘影响 |
| B6 | Agent file | Browser storage | 完整安装端发送 |
| B7 | Browser File | Agent file | 完整安装端接收 |
| F0 | Browser WebRTC | Browser WebRTC | 现有回退基线 |

网络场景：

- 千兆有线 LAN；
- Wi-Fi 同 AP；
- IPv4；
- IPv6；
- UDP 被防火墙阻断；
- WebTransport 不支持；
- Local Network Access 权限允许/拒绝；
- Agent 运行/未运行/版本不匹配。

文件规模：1MiB、64MiB、1GiB、10GiB、50GiB。内存基准不得一次分配测试总大小。

## 13. 性能规则

1. 文件热路径不使用 JSON。
2. 不为每个 block 创建 Tokio task、线程、channel 或 `Vec`。
3. 不在每个 block 上调用 `tokio::fs` 或 `spawn_blocking`。
4. 文件线程在传输期间长期存在；文件一次打开，任务结束才关闭。
5. 所有网络与文件队列有明确容量。
6. 默认 4 lane，不盲目增加到几十条流。
7. 目标文件在接收数据前预分配。
8. 不为每块计算应用层哈希或输出日志。
9. 性能统计最多每秒更新一次，UI 进度最多每 250ms 合并一次。
10. 性能测试使用 release 构建，并记录浏览器、系统、网卡、磁盘、连接类型和 `iperf3` 同方向结果。

## 14. 决策门

- Chrome 142+ LNA HTTP/TCP 是默认高速数据面；六上/六下 WebTransport 是仅供不支持 LNA 的浏览器使用的兼容路径，不再采用六上/一下策略。
- 任何会话都不能出现两个 Agent；检测到远端也报告 Agent 时，不协商 Agent ↔ Agent，而是仅保留一端 Agent 或退回网页模式。
- 两端均为手机时固定使用现有 WebRTC，原生项目不参与。
- 高级功能默认不进入热路径；只有 benchmark 证明吞吐、CPU、内存和复杂度没有实质退化时才允许加入。
- 不开发安装器、托盘、后台服务或开机启动；发布能力只围绕单文件 EXE、代码签名、下载校验和当前用户协议注册展开。
- B0/B1 满速而 B6/B7 慢：优化文件 I/O 或浏览器存储，不改网络协议。
- B0/B1 本身慢：先比较 LNA HTTP/TCP 与 OpenSpeedTest，再检查浏览器 XHR、TCP socket、CPU 和网卡；WebTransport 只分析兼容路径。
- LNA 权限拒绝、LNA 端点失败和 LNA API 不受支持是三个不同状态，代码、日志和 UI 必须分别处理。

## 15. 当前仓库状态说明

错误的 `crates/winrisef-transfer` 双原生 scaffold 已在 2026-07-18 删除并重建为：

- `winrisef-core`：协议、extent、coverage 和固定 buffer pool；
- `winrisef-agent`：无前端 WebTransport server、短期证书、鉴权和双向 memory engine；
- `docs/protocol-v3.md`：当前网页和 Agent 共用的双方向六 connection WebTransport 兼容 memory benchmark 协议。
- `docs/lna-http-v1.md`：Chrome 142+ 默认六路 HTTP/TCP memory benchmark API。

第一阶段代码已经完成格式化、workspace check、Clippy `-D warnings` 和测试构建验证，没有启动 Agent、执行测试用例或运行传输。真实浏览器互操作、吞吐与 `iperf3` 门槛仍处于未验证状态，必须在 Phase 2 修改现有网页后完成。

2026-07-18 已补齐第一个可由用户手工执行的浏览器互操作闭环：Windows `winrisef://` 当前用户注册、可信 Origin 白名单、HTTPS fragment 回调、一次性 launch token、持久 Local Bridge、一次性 peer ticket、现有 WebRTC ticket 请求/响应，以及远端纯网页四 lane 双向内存测速入口。Agent 仍无 GUI，安装端网页仍不承载 benchmark payload，双方同时发布 Agent 时按 device ID 只保留一端。该闭环尚未由 Codex 启动或测试，吞吐、浏览器兼容性、防火墙行为和 `iperf3` 90% 门槛由用户下一步手工验证；真实文件选择和磁盘 I/O 尚未进入实现。

2026-07-19 首次手工互操作已确认网页可以通过 `winrisef://` 启动 Agent、打开 HTTPS 回调页并将凭据交还主页面，但 Chrome 在连接 `127.0.0.1:17691` 的本机 Bridge 时报告 `ERR_QUIC_PROTOCOL_ERROR` / `Opening handshake failed`。排障阶段临时启用每进程全局 trace 日志，固定保存至用户 Documents 的 `WinriseF-Agent-Logs`；同时以显式 Quinn Incoming → TLS → HTTP/3 → WebTransport CONNECT 接受链替换会吞掉握手错误的 server 包装。定位并修复根因后必须降低日志级别并移除非必要诊断噪声。

2026-07-19 首次性能实测确认 native benchmark 确实通过手机 `192.168.31.207` 直连 Agent `192.168.31.202:17691`，但 browser→Agent 仅约 51Mbps、Agent→browser 仅约 145Mbps，未达到同机 OpenSpeedTest 218/278Mbps 基线。根因审计发现全局同步 trace 在两次测试中产生约 53.5 万行/96MB QUIC 热路径日志，且 64MiB Agent→browser 测试因 64MiB extent 只有一条 lane 承载 payload。benchmark 协议升为 v2：默认日志降为低频分级事件，Agent 发送侧使用 BBR，stripe 改为 16MiB 并按 lane 确定性分配，浏览器吞吐计时排除 CONNECT/握手。聊天附件保持 WebRTC，只有用户复测双向均达到同方向基线 90% 后才能开始真实附件接入。

2026-07-19 v2 热路径二次审计进一步移除整流量内存复制：Agent→browser 改为共享 4MiB immutable `Bytes` 直接进入 Quinn，browser→Agent 改为 ordered `read_chunk` 零拷贝计数；browser 每 lane 有界并行两个 write，WebTransport 显式请求 throughput 拥塞策略。Agent BBR 使用 1MiB initial cwnd 与 10ms LAN initial RTT，并在每次 payload 后输出包含 RTT、cwnd、丢包、MTU、UDP I/O batching 和 flow-control blocked frame 的单条 QUIC summary。该版本只完成编译/源码类型验证，性能仍必须由用户用 1GiB 双向手工复测确认。

2026-07-19 v2 的 1GiB 复测显示 browser→Agent 稳定约 48.9Mbps，但 Agent 侧 RTT 5.36ms、零丢包、零拥塞和零流控阻塞，证明瓶颈位于手机 Chrome 的单 QUIC connection 发送端；Agent→browser 的 64MiB 已达到 243.8Mbps，而 1GiB 在约 16MiB 后停止，定位到 browser 先等待收齐四条 incoming stream、再开始读取造成的接收窗口互锁风险。对照克隆到 `.firecrawl/openspeedtest-speed-test` 的 OpenSpeedTest 官方源码，其默认用六路并行 XHR/HTTP 请求、30MB upload Blob 和 300ms stagger 聚合带宽。benchmark 协议因此升为 v3：六张独立一次性 ticket 建立六条独立 WebTransport/QUIC connection，逻辑总量均分并聚合计时；browser 在每条 incoming stream 到达时立即开始消费，禁止等待收齐后再读。

2026-07-19 v3 的 1GiB 复测达到 browser→Agent 约 146Mbps、Agent→browser 约 207Mbps。六路上传 session 各约 24Mbps，Agent 端无丢包、无拥塞和无流控阻塞，说明多 connection 确实绕过了一部分 Chrome 单 QUIC sender 上限；当时据日志临时改成六上/一下，但随后跨两个网络的对照测试明确否定了该策略：上一版六上/六下在网络一为 141/214Mbps、网络二为 345/891Mbps，六上/一下分别只有 125/187Mbps 和 321/724Mbps。因此 WebTransport 兼容路径恢复为双方向六 connection。

2026-07-19 正式性能架构改为 Chrome 142+ LNA 优先：从 `https://e.winrisef.top` 经用户 Local Network Access 授权，纯网页直接请求 Agent 的明文 HTTP/1.1/TCP 数据 API，以六个并发 XHR worker 和约 30MiB 有界请求循环逼近 OpenSpeedTest。用户拒绝 LNA 时极速模式不可用；只有 Permissions API 不识别 LNA permission 名称时才回退上述六上/六下 WebTransport，之后才保留普通 WebRTC。当前阶段先实现独立双向 memory benchmark，达到性能门后再接入真实聊天附件。

2026-07-19 LNA memory benchmark 已完成代码接入：Agent 在与 UDP/QUIC 相同的数字端口监听 TCP，提供精确 Origin/CORS/LNA、一次性 ticket、64MiB 单请求硬上限、六 active request 上限和有界流式内存实现；启动回调与 WebRTC capability 同时发布 HTTP 和 WebTransport endpoints。远端网页使用 Chrome LNA permission probe、六个 XHR worker、30MiB 请求循环和每请求一张 ticket；`denied` 不进入 QUIC，permission descriptor 不受支持才进入恢复后的六上/六下 v3。

2026-07-19 正式文件接入已完成代码实现：协议升级到 LAN V11 与 Bridge V2，新增 opaque source registry、系统文件选择/保存、全局单 active transfer、`.part`/sync/同文件系统原子完成，以及独立于 benchmark 的 Native File V1。正式 LNA 路径使用六 XHR、最大 30MiB segment 和 4MiB 池化 positional I/O；正式 WebTransport 回退使用六 connection、每 connection 四 lane、64MiB extent 和最多 24 个 4MiB buffer。网页通过独立 local-agent/peer-bulk ports 把 native 编排注入现有聊天 Runtime；用户开启极速模式且 Agent 连接成功后，`>=64MiB` 普通文件默认自动尝试极速，不再要求额外部署特性开关，图片、语音和小文件仍走 WebRTC。LNA 拒绝、unsupported 和 endpoint failure 保持不同语义。代码阶段只执行静态/编译验证；Codex 不启动 Agent、网页、浏览器、测试用例或吞吐测速，双向 64MiB/1GiB/10GiB 与 SHA-256 由用户手工验收。

2026-07-20 完成 V10/V11 新增链路清理：删除未消费的 `transfer-started`、事件冗余总量、grant 回显的固定/可推导字段、网页端重复队列状态和无引用 fixture 包装；合并 Bridge 关闭、LNA endpoint 选择、十六进制解析与 positional I/O。鉴权、单 active transfer、精确 coverage、取消清理、`.part`、sync 和原子完成保持不变。Rust Clippy、workspace tests、网页 TypeScript 和跨仓库 fixture 检查通过；未启动 Agent、网页或真实传输。

2026-07-22 修复公网 IPv6 热点下的启动竞态：Windows Agent 不再在启动 Bridge 前同步等待 PowerShell/UAC，而是在 UDP/TCP 已绑定、HTTPS callback 已打开后异步授权防火墙；Bridge V3 snapshot 新增 `publicIpv6State`，私有 IPv4/CGNAT/ULA 立即可用，GUA 只在授权成功后发布。protocol launch 增加 per-user 单实例 mutex；网页启动改为 single-flight，保留未过期 nonce、接受迟到的合法 callback，并在 launching/connecting 阶段禁用重复开关。防火墙日志分别记录规则查询和 elevation 耗时。

## 16. Version Control V1（已实施）

版本控制器作为与 Attachment Transfer 并列的本机有界上下文实施，不复用 Bridge V3 的传输语义：

- 自定义协议增加 `feature=transfer|version-control`，默认 transfer 行为保持不变；
- version-control 只启动 loopback WebTransport 与 Bridge V1，一次只授权一个仓库，并与 `/t` 互斥；
- 新增 `winrisef-version-control` crate，以 vendored libgit2 提供只读历史、引用、stash、工作区分组、冲突视角、任意 revision diff、预览和导出；
- 控制 JSON 固定 64KiB 上限，历史与文件清单按实际序列化预算分页，正文使用独立数据流；
- 浏览器正式入口为 `/toolbox/version-control`，交互基线为 CtxRun 的提交图、右键比较、Esc 恢复、文件树与 Monaco Diff；
- 历史比较采用短窗口合并、单任务串行和 latest-generation 丢弃，搜索/分页也合并同页请求并忽略旧响应；Agent 仅保留最近三份单份元数据的轻量 Diff 会话，历史比较不查询当前 worktree status；打开比较不再预生成完整 patch，GitPatch 改为导出时建立路径索引并按需生成，命中时不读取两侧全文；
- 右侧文件树按路径 Map 线性构建并预计算目录选择范围；Monaco 大文本 Diff 使用 30 秒有界计算并关闭高成本换行布局；
- 写入只允许用户显式发起的导出，且仓库内目标需要二次确认并在完成后刷新状态。

Git V1 不提供任何分支切换、远程网络、index/commit/reset/restore 或 stash 变更命令。SVN 是下一独立阶段，届时再决定 `svn.exe`、服务器访问、凭据和 branches/tags 目录语义。

## 17. 参考依据

- WebTransport W3C Working Draft：`https://www.w3.org/TR/webtransport/`
- MDN WebTransport API：`https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API`
- Chrome Local Network Access：`https://developer.chrome.com/blog/local-network-access`
- `web-transport-quinn` 文档：`https://docs.rs/web-transport-quinn/latest/web_transport_quinn/`
- `wtransport` 文档：`https://docs.rs/wtransport/latest/wtransport/`
- Quinn 文档：`https://docs.rs/quinn/latest/quinn/`
