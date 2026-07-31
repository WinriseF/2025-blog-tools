# WinriseF Toolbox Agent：项目架构与工作说明

更新日期：2026-07-31

适用仓库：`E:\Project\PROJECT\2025-blog-tools`

关联网页：`E:\Project\PROJECT\2025-blog-public`

产品标识：`WinriseF Toolbox`；Windows 本机组件：`WinriseF Toolbox Agent`；当前发布版本：`v0.1.0-beta.1`。完整命名和版本规则见 [`docs/product-identity.md`](docs/product-identity.md)。

## 1. 阅读顺序与权威性

任何开发者或 AI Agent 开始工作前，按以下顺序阅读：

1. 根目录 `README.md`；
2. 本文件 `agent.md`；
3. 与任务对应的 `docs/*.md` 协议契约；
4. 网页仓库 `AGENTS.md` 和 `ARCHITECTURE.md`；
5. 与当前任务直接相关的 Rust/TypeScript 源文件与 fixture。

本文件定义产品边界和模块职责；协议细节以对应的版本化 `docs/*.md` 与 fixture 为准；实现行为以当前代码为准。历史计划和一次性排障记录不再作为规范保留。“一次会话永远最多一个 Agent”是永久产品约束，不得把双端安装作为隐藏优化、实验模式或未来升级方向。

“安装端”只是历史上的拓扑简称，实际产品不使用安装器。Windows Agent 必须保持为单个便携 EXE：首次无参数双击在当前用户范围注册自身路径并打开官网确认页，随后退出；禁止复制自身、创建服务、加入开机启动或常驻托盘。移动或重命名后由用户再次双击修复注册。

## 2. 第一架构不变量

本项目不是两个原生客户端之间的文件传输器。

唯一批准的正式拓扑：

```text
安装端设备 A                              纯网页设备 B
┌──────────────────────────────┐          ┌──────────────────────┐
│ 2025-blog-public 网页 A      │          │ 2025-blog-public 网页 B
│  UI、会话、聊天、状态        │          │ UI、File/Storage API │
│             ⇅ localhost      │          │          │           │
│ Rust Agent A                 │◀─────────│──────────┘           │
│ 原生 I/O + HTTP/WT Server    │ LNA HTTP/TCP；兼容 WT/QUIC      │
└──────────────────────────────┘          └──────────────────────┘
```

强制规则：

- 一次会话只有设备 A 安装和启用 Agent；设备 B 不安装任何程序。
- 即使 A、B 都是桌面电脑，也只能选一端启用 Agent，禁止 Agent ↔ Agent。
- 如果 A、B 都是手机，则完全使用现有网页/WebRTC，Rust Agent 不参与。
- 浏览器 B 始终主动连接；默认通过 Chrome LNA 请求 Agent 的 HTTP/1.1/TCP 数据 API，不支持 LNA 时才创建 WebTransport session。
- 浏览器不能作为 server，也不能直接使用 raw Quinn/raw TCP；XHR/fetch 只能访问 Agent 暴露的受限 HTTP API。
- 网页 A 是 Agent 的 UI/control plane，不是大文件 payload 中继。
- Rust 工程不包含网页、桌面窗口或产品前端；唯一前端是现有 `2025-blog-public`。
- Agent A 发送或接收增强文件时，文件字节不得经过网页 A 的 JS heap。
- 现有 WebRTC 必须继续可用，承担会话建立、控制、聊天和增强失败回退。
- 用户明确拒绝 LNA 时极速模式不可用；不得用 WebTransport 绕过拒绝。只有 permission descriptor 不受支持才回退六上/六下 WebTransport。

如果一个实现需要网页 B 安装程序、需要网页 B 监听端口、允许两个 Agent 直连、在 Rust 仓库增加前端，或需要文件 payload 从 Agent 绕回网页 A，它就是错误架构。

## 3. 系统上下文

### 3.1 网页系统

网页项目是现有产品和 UI 的唯一来源。它已经实现：

- `/t` LAN Session V12；
- Supabase Realtime Presence/Broadcast 配对；
- WebRTC DataChannel；
- 聊天、附件、进度、取消和同页恢复；
- 浏览器 Memory、OPFS、IndexedDB 和直接文件存储；
- 多附件有界发送调度；
- WebRTC 断线恢复状态机。

不要在 Rust 仓库复制 React 状态机、聊天历史、二维码、Supabase client 或浏览器存储策略。

### 3.2 Rust Agent

Agent 是无独立主窗口的本地能力提供者：

- 接受网页 A 的 loopback 控制连接；
- 调起系统文件选择/保存对话框；
- 打开文件并执行 positional I/O；
- 为网页 B 提供 LAN HTTP/TCP 默认数据 API和 WebTransport 兼容 server；
- 执行有界 buffer、extent 和 lane 调度；
- 将低频状态与进度报告给网页 A；
- 管理短期证书、launch token 和 peer ticket；
- 不连接 Supabase，不保存聊天历史，不持有网页账号凭据。

Agent 不提供网页、WebView、桌面 UI 或独立聊天界面。允许的用户交互只有首次双击后打开现有官网结果页、由现有网页触发的系统文件选择/保存对话框和明确授权对话框，以及浏览器和系统权限流程。

Windows 原生对话框统一通过 `native_dialog` 适配层打开。EXE 清单启用 Per-Monitor V2 DPI 感知和 Common Controls v6；目录/文件/保存继续使用 Windows Common Item Dialog，明确授权使用现代 Task Dialog，不回退到旧式 `MessageBox`。适配层只在对话框存在期间创建一个离屏、无任务栏入口的临时 owner window，使无主窗口 Agent 发起的对话框稳定位于浏览器前方；确认、取消或错误返回后由 RAII 立即销毁 HWND。该 owner 不引入常驻窗口、消息循环或后台线程。

### 3.3 基础设施

- Supabase 只做浏览器间的 Presence/Broadcast 信令。
- WebRTC 是现有浏览器会话和回退 transport。
- LNA HTTP/TCP 是 Agent 与纯网页之间的默认高速数据面；六上/六下 WebTransport 只是不支持 LNA 时的兼容数据面。
- EdgeOne Pages Blob 公共中转功能独立存在，不属于本地 Agent 数据面。

## 4. 有界上下文

### 4.1 Session Bootstrap

负责：

- 自定义协议启动；
- 便携 EXE 首次自注册与官网成功/失败回执；
- loopback endpoint 与证书 hash 交付；
- launch token；
- bridge 版本握手；
- Agent capability。

Windows `launch` 必须先绑定 UDP/TCP、打开 HTTPS fragment 回调并允许 loopback Bridge 鉴权，再异步检查或申请公网 IPv6 防火墙规则。防火墙 UAC 不得阻塞本机 Bridge 启动；私有 IPv4、CGNAT 和 ULA endpoint 可先发布，GUA IPv6 只有在规则状态为 `available` 后才能进入 Bridge V3 endpoint snapshot。一次用户会话只能存在一个 protocol-launch Agent，重复启动不得竞争固定端口或覆盖仍有效的 launch nonce。

不负责文件传输协议或聊天业务。

### 4.2 Peer Authorization

负责：

- 创建短期 peer ticket；
- 绑定浏览器设备 ID、Agent session ID、权限和过期时间；
- HTTP/WebTransport 的精确 Origin 与一次性 ticket 校验；
- 防重放和连接关闭。

不负责 Supabase 登录或 WebRTC SDP。

### 4.3 Attachment Transfer

负责：

- attachment manifest；
- accept/reject/cancel/complete；
- lane/extent 分配；
- 数据覆盖范围和最终长度；
- 进度事件；
- 临时文件完成策略。

不负责聊天消息排序、UI 卡片或 React 状态。

### 4.4 Platform File I/O

负责：

- 系统文件选择器；
- 文件句柄生命周期；
- 预分配；
- 按 offset 读写；
- flush/sync 和原子改名；
- 平台错误标准化。

不负责网络协议和附件业务状态。

## 5. Clean/Hexagonal 依赖规则

依赖只能向内：

```text
main/bootstrap
    ↓
adapters（WebTransport、文件、launch、metrics）
    ↓
application use cases
    ↓
core domain + ports
```

### 5.1 Core 可以知道

- `SessionId`、`PeerId`、`AttachmentId`；
- `AttachmentManifest`、`Extent`、`LaneId`；
- transfer state 和合法状态转换；
- protocol version 和固定 header；
- `PeerTransportPort`、`FileSourcePort`、`FileSinkPort`、`EventPort`、`ClockPort`；
- buffer/extent 调度的纯逻辑。

### 5.2 Core 不得知道

- Tokio task、Quinn connection、WebTransport session；
- rustls/rcgen 证书对象；
- `rfd` 对话框；
- Windows、Linux 或 macOS 文件 API；
- `serde_json::Value`；
- React、Supabase、WebRTC；
- CLI 参数或日志格式。

### 5.3 Adapter 规则

- adapter 负责把外部格式映射为 core 类型。
- WebTransport adapter 不包含附件 UI 规则。
- file adapter 不发送网络控制消息。
- launch adapter 不访问任意用户文件。
- application use case 只依赖 ports，不构造具体 adapter。
- 测试通过 fake/memory ports 驱动 use case，不要求真实网络或磁盘。

## 6. 当前 Rust 目录

Cargo workspace 只包含四个 crate：

- `winrisef-core`：二进制协议、extent 覆盖检查与调度的纯逻辑；
- `winrisef-platform`：Windows 相关平台能力；
- `winrisef-agent`：启动、证书、认证、Bridge、LNA/HTTP、WebTransport、文件 I/O、系统对话框、诊断和 Version Control 适配器；
- `winrisef-version-control`：vendored libgit2 的只读 Git 内核。

`crates/winrisef-transfer` 不是 Cargo workspace member，也不是当前产品路径；不要向其中增加功能。`winrisef-agent/src` 目前按运行时边界以扁平模块拆分，新增模块应遵循职责边界，而不是回填早期规划中的空目录图。

## 7. 网页集成边界

正式网页代码位于 `E:\Project\PROJECT\2025-blog-public`。修改前必须遵守该仓库的 `AGENTS.md`，并在架构变化时更新其 `ARCHITECTURE.md`。

### 7.1 复用的接口

- `LanConnectionTransport`：现有通用连接写入/backpressure 边界；
- `LanConnectionRuntime`：每个远端设备的业务核心；
- `LanAttachmentSendScheduler`：现有 WebRTC 附件调度；
- `LanStorageEngine`：纯网页接收文件的存储端口；
- `LanCapability`：协商浏览器和增强能力；
- `useLanTransferController`：最终装配入口。

### 7.2 当前网页适配器

```text
src/lib/lan-transfer/native-agent/
├─ capability.ts
├─ endpoint-validation.ts
├─ launch-client.ts
├─ local-bridge.ts
├─ native-storage-writer.ts
├─ peer-lna-http.ts
├─ peer-native-file.ts
├─ peer-webtransport.ts
├─ ports.ts
├─ types.ts
└─ webtransport.ts
```

约束：

- `native-webrtc-transport.ts` 继续只负责 WebRTC，不塞入 WebTransport 分支。
- React hook 不解析二进制 extent header。
- `local-bridge.ts` 只连接 Agent，不读取大文件 payload。
- `peer-webtransport.ts` 只存在于纯网页 B，负责连接 Agent 和 streams。
- `peer-native-file.ts` 通过现有 Runtime 事件更新附件状态，不建立第二份聊天状态。
- 新增增强模式不能改变未安装 Agent 用户的默认路径。
- 网页仓库是唯一前端；Rust 仓库不得创建 benchmark 网页、管理页面或桌面窗口。

## 8. Transport 选择规则

每个 peer 会话有控制面和按能力选择的数据面：

```text
Control/chat transport: WebRTC DataChannel（始终保留）
Primary bulk transport: Chrome LNA HTTP/1.1/TCP 多 XHR（仅私有 IPv4/ULA）
Compatibility bulk transport: WebTransport/QUIC 六上/六下（LNA descriptor 不支持，或已授权的公网 IPv6 endpoint）
```

附件选择逻辑：

1. 本端 Agent 已连接；
2. LNA 允许时，私有 IPv4/ULA 的 LNA probe 已成功；若 descriptor 不支持，或 LNA 已拒绝但存在已授权的公网 IPv6 endpoint，则远端必须支持 WebTransport；
3. 已选 LNA 或六路 peer WebTransport 已鉴权并 ready；
4. 附件类型/大小满足增强策略；
5. 用户没有关闭原生增强；
6. 选择 `native-lna-http` 或 `native-webtransport`；没有可用数据面、端点故障或增强关闭时使用 `webrtc`。LNA 拒绝绝不允许私有 HTTP 兜底，但不会阻止已授权的公网 IPv6 WebTransport。

第一版只将单个普通大文件送入增强路径；文字、语音、小图片继续 WebRTC，减少双数据面状态复杂度并优先保证峰值吞吐。

禁止在正在发送的文件中途把剩余 block 静默切换到另一个 transport。第一版应明确失败并提供重试；后续只有在实现统一 extent checkpoint 后才允许恢复到另一数据面。

断点续传、多文件公平调度、跨会话恢复、传输历史和逐块校验都不是首发要求。只有独立 benchmark 证明它们不会降低峰值/持续吞吐、不会破坏有界内存且不会增加热路径分配时，才允许进入实现计划。

## 9. WebTransport session 模型

### 9.1 连接角色

- Agent：server；
- 纯网页：client；
- 网页 A：loopback client；
- 不存在“浏览器作为 server”。

### 9.2 Stream 布局

每个 peer WebTransport session：

- 一个可靠双向 control stream；
- 每个 active attachment 默认四个由发送方创建的可靠单向 data streams；
- 第一版同一 session 只允许一个 active enhanced attachment；
- datagram 不用于文件字节或关键控制。

### 9.3 Frame 规则

- control stream：长度前缀 + 有上限的版本化 JSON；
- data stream：固定二进制 stream header + 多个 extent header + 原始 payload；
- 所有整数明确端序；建议 network byte order/big-endian；
- header 解码先检查 magic、major、长度上限、lane 范围和 offset 溢出；
- 未鉴权连接不得接受 data stream；
- 不为每个 4MiB block 创建 JSON、哈希或 ACK；
- 最终 complete 只有在目标 storage/file 完成 flush 和长度覆盖检查后发送。

### 9.4 调度常量

默认值集中定义，不在多个 adapter 重复硬编码：

```text
LANES = 4
EXTENT_SIZE = 64 MiB
IO_BLOCK_SIZE = 4 MiB
BUFFERS_PER_LANE = 2
```

浏览器可以把 4MiB I/O cache 分成更小的 stream writer 写入，但 extent 和文件 offset 语义不得改变。

以上 `EXTENT_SIZE` 是正式文件数据面常量。WebTransport 兼容 memory benchmark v3 在两个方向都把逻辑总量拆到六条独立 QUIC connection；每条 connection 单独使用 16MiB stripe 并按 lane ID 确定性分配。不得把测速 stripe 反向写入正式文件协议。

## 10. 文件 I/O 规则

### 10.1 安装端读取

- 文件由 Agent 的系统选择器获得，不接受网页提供任意绝对路径。
- 每个 lane 使用长期存在的专用同步 worker 或等价的有界平台实现。
- worker 按 offset 读取到池化 4MiB buffer。
- 网络发送完成后 buffer 返回池中。
- 不在 block 循环中 reopen 文件、分配新 Vec 或创建线程。

### 10.2 安装端写入

- 用户通过系统保存对话框确认目标。
- 写入 `<name>.part`，数据到达前设置最终逻辑长度并尽可能预分配磁盘空间。
- 四 lane 按 offset 写入，同一 offset 不得被两个 extent 重复拥有。
- 完成前检查总长度与 extent coverage。
- 完成后 flush/sync，再原子改名为目标文件。
- overwrite、取消和失败的 `.part` 策略必须显式，不得静默覆盖用户文件。

### 10.3 纯网页端

- 发送读取复用约 4MiB `File` cache，再切分成 WebTransport writer 接受的块。
- 接收继续使用现有 `LanStorageEngine` 选择 Direct File、OPFS、IndexedDB 或 Memory。
- 遵守 Streams API backpressure，不把完整文件读入 ArrayBuffer。
- 浏览器端限制由 capability 明确展示，不把“有 Agent”误认为两端都是原生 I/O。

## 11. 安全规则

### 11.1 证书

- 使用 rustls 支持的明确 crypto provider。
- Agent 生成 P-256 短期证书，默认每次进程启动轮换。
- certificate-hash 证书有效期最多两周，项目取不超过 13 天。
- 浏览器使用 `serverCertificateHashes` 校验证书 SHA-256。
- 禁止在生产代码中实现 skip verification 或 trust-all verifier。

### 11.2 Token

- launch token 与 peer ticket 使用 CSPRNG，至少 128 bit。
- 默认一次性、短有效期；验证成功即消费。
- token 使用常量时间比较或摘要后比较。
- peer ticket 绑定 session、peer、权限和 expiry。
- token 不进入日志、query string、Supabase 明文或 localStorage。

### 11.3 Origin 与权限

- loopback 和 LAN WebTransport 都检查精确 Origin allowlist。
- 开发 origin 与生产 origin 分开配置，生产包不默认信任任意 localhost 页面。
- 浏览器 Local Network Access 权限拒绝是正常可恢复状态，不诱导用户关闭浏览器安全功能。
- Agent 只绑定必要地址和端口；UI 明确说明局域网/防火墙需求。

### 11.4 输入验证

- 所有网络长度在分配前校验上限。
- offset + length 使用 checked arithmetic。
- attachment ID 必须属于已授权 session。
- 文件名只作为显示元数据，不能直接拼接路径。
- 未知协议 major、重复 lane、重复 active attachment、越界 extent 立即关闭对应 session。

## 12. 性能约束

热路径禁止：

```text
tokio::fs per block
spawn_blocking per block
Vec::new per block
serde_json per block
thread/task/channel per block
debug log per block
hash per block
unbounded channel
```

要求：

- 网络 runtime 与磁盘 worker 分离；
- buffer pool 和 channel 全部有界；
- Native File V1 的 4MiB buffer 由 active transfer 按需创建并循环复用：LNA HTTP 上限为 `6 × 2 = 12` 个（48MiB），WebTransport 上限为 `6 × 4 = 24` 个（96MiB）；segment/extent 结束不得重新分配或重新清零同尺寸 buffer，transfer 结束统一释放；
- progress 原子累计，Rust 最多每秒格式化一次；
- 网页 UI 进度合并后更新，避免每个 stream write 触发 React render；
- release benchmark；
- 一次只调整一个变量并记录结果；
- raw QUIC/TCP 结果只能解释上限，不能推导浏览器可用性。

## 13. 错误与状态

统一错误分类：

- `unsupported`：浏览器/API 不支持；
- `permission_denied`：LNA、系统文件或防火墙权限；
- `agent_unavailable`：Agent 未运行或 loopback 不可达；
- `peer_unreachable`：LAN endpoint/UDP/地址族问题；
- `tls_certificate`：hash 或证书错误；
- `authentication`：token/ticket/Origin 错误；
- `protocol`：版本、header 或状态机错误；
- `file_io`：打开、预分配、读写、sync、rename；
- `timeout`：握手、backpressure、final confirmation；
- `cancelled`：本地或远端明确取消；
- `internal`：不应发生的 invariant 失败。

错误进入网页时必须是稳定 code + 用户可读 message；底层库错误只作为本地诊断 cause，不直接展示私密路径或 token。

## 14. 测试边界

### 14.1 Core 单元测试

- 状态转换；
- extent 不重叠且完整覆盖；
- header round-trip 与畸形输入；
- offset 溢出；
- ticket expiry/消费；
- fake transport + memory file ports 的 send/receive use case。

这些测试不得需要真实 socket、文件对话框或浏览器。

### 14.2 Adapter 测试

- loopback WebTransport 互操作；
- TypeScript/Rust protocol fixtures；
- certificate hash；
- file positional I/O 和预分配；
- 取消、对端关闭和 partial file；
- bounded backpressure。

### 14.3 网页测试

- feature detection；
- Agent 未安装；
- launch fragment 一次性消费并从地址栏清除；
- WebTransport 失败后 WebRTC 会话仍可用；
- storage engine 与 bulk adapter 对接；
- 旧纯网页流程不受影响。

### 14.4 验证纪律

- 用户要求“只验证不运行”时，只进行格式化、静态检查、编译、Clippy 和测试构建；不得启动 Agent、receiver、sender、benchmark、浏览器自动化或真实网络传输。
- `cargo test --no-run` 只构建测试；`cargo test` 会运行测试，不能混淆。
- 网页仓库当前 `AGENTS.md` 明确禁止为验证主动运行 `pnpm`/`npm` scripts，除非用户明确授权。
- 不用“能编译”替代跨语言 protocol fixture 和后续真实设备验收。

## 15. 代码质量规则

- Rust 1.97.1，edition 2024。
- `unsafe` 默认禁止；必须使用时先写安全不变量并隔离在平台 adapter。
- library error 使用 `thiserror`；binary 装配层可使用 `anyhow`。
- 不 `unwrap` 网络输入、用户输入或文件 I/O 结果。
- 不建立超过 1000 行的源文件；接近该规模时按职责拆分。
- 常量集中；协议字段和错误码不得在 Rust/TypeScript 两侧随意复制不同值。
- 注释解释“为什么”和安全/性能不变量，不复述代码。
- 不为未进入当前 Phase 的未来功能创建大量空抽象。
- 不保留错误架构的兼容路径；项目允许 breaking protocol upgrade。

## 16. 修改范围规则

- 用户要求分析、审查或规划时，不实施功能代码。
- 用户要求某一 Phase 时，只实现该 Phase 及其必要基础。
- 修改 Rust 仓库不自动授权修改网页仓库，反之亦然；根据用户明确范围行动。
- 不删除或覆盖用户已有未提交改动。
- 不执行安装、发布、开放防火墙、注册系统协议或生成真实证书，除非该动作处于用户明确要求的实施范围。
- 任何可能改变正式拓扑的决定先更新本文件和受影响的协议契约，再编码。

## 17. 当前仓库状态

当前 Cargo workspace 包含：

- `winrisef-core`：不依赖 Tokio/Quinn 的协议与调度核心；
- `winrisef-platform`：Windows 平台能力；
- `winrisef-agent`：唯一的无前端 Agent server；
- `winrisef-version-control`：独立的只读 Git 内核；
- `docs/protocol-v3.md`：当前网页与 Agent 的双方向六 connection WebTransport 兼容 memory benchmark 协议契约；
- `docs/lna-http-v1.md`：Chrome 142+ 默认 LNA HTTP/TCP memory benchmark API 契约；
- `docs/native-file-v1.md`：LAN V12、Bridge V3、正式 LNA/WT 文件协议和文件生命周期契约；
- `docs/version-control-v2.md`：Git/SVN 只读 Bridge V2 契约。

当前实现保留 LNA HTTP/TCP 与六连接 WebTransport memory benchmark，并已实现正式 Native File V1：Bridge V3 系统选取/保存、动态 endpoint snapshot、opaque source、全局单任务、`.part`/sync/原子完成、六 XHR/30MiB LNA 数据面，以及六 connection × 四 lane/64MiB extent 的 WebTransport 回退。正式文件数据面使用 active-transfer 级惰性有界 buffer pool，LNA 的 12 个与 WebTransport 的 24 个 4MiB buffer 都在后续 segment/extent 中原位复用。Windows protocol launch 使用单实例互斥；本机 Bridge 先启动，公网 IPv6 防火墙授权随后异步完成，GUA endpoint 只在授权成功后发布。网页的 WebRTC V12 是控制面；只有 `>=64MiB` 普通文件进入 native，图片、语音、小文件继续 WebRTC。LNA `denied` 不阻止已授权的公网 IPv6 WebTransport；其他路径失败保持 WebRTC 回退。该状态描述不代替当前设备上的人工端到端/吞吐验收。

## 18. Version Control 本机只读上下文

Agent 支持 `winrisef://launch?...&feature=version-control`。该模式与 transfer 共用单实例互斥，但只绑定 `127.0.0.1` WebTransport 与 `/winrisef/version-control/v2`，不初始化 LAN HTTP、文件传输、地址发现或防火墙授权。重复启动通过回调返回稳定的 `agent_busy`。

`crates/winrisef-version-control` 继续作为独立 Git 读取内核，使用仅启用本地能力的 vendored `git2/libgit2`，不启用 SSH/HTTPS/OpenSSL，也不依赖 `git.exe`。SVN 由 Agent 的 `svn_cli`/`svn_repository` 适配器调用系统 `svn.exe`，不读取 `.svn/wc.db`、不经过 shell，并固定 `--non-interactive --no-auth-cache`；Windows 上所有 SVN 子进程统一以 `CREATE_NO_WINDOW` 后台启动。普通命令的 stdout/stderr 并发排空且维持 45 秒超时；全量 `svn diff --git` 改为 64KiB 固定缓冲的增量解析，独立使用 120 秒和 512MiB 处理预算，该预算不会预分配或常驻同等内存。网页只能以 Agent 生成的 repository/diff/file/export ID 调用；目录和保存目标只由 `rfd` 系统对话框产生。控制帧维持 64KiB 并按序列化预算分页，源码预览走独立单向流。目录同时包含 Git 和 SVN 时，Agent 返回候选 ID，网页必须显式选择后才打开。

只读范围包含历史、HEAD/本地与已有远程引用、标签、stash、HEAD reflog 删除分支提示、工作区与冲突 stage。禁止 checkout/switch、fetch/pull/push、stage、commit、restore/reset 和 stash 写操作。导出是唯一写入例外：系统保存框、仓库内二次确认、同目录临时文件、sync 与原子完成，失败、取消或 Bridge 会话退出时清理临时文件。

Git V2 支持普通仓库、linked worktree、bare 和 gitlink；SVN V2 支持工作副本检测、状态、混合版本提示、显式确认后的线性历史和只读文本差异预览。SVN revision 使用强类型入口：`empty` 映射 r0、commit 必须为十进制 `r<N>`，working tree 只允许位于右侧。工作区 Diff 同时用 status 补充未跟踪/冲突信息、用 summarize 保留删除目录等权威节点类型；每次 Diff 只运行一次全量 `svn diff --git`，流式统计全部文件的行数和二进制元数据，但单文件只保留最多 2MiB Patch，三个 revision range 共享 32MiB Patch 缓存。“仅变更”预览优先命中该缓存；未常驻的文件只在点击时执行一次按路径限定的 `svn diff`，结果进入当前 Diff Session 的预览缓存。完整文件模式才并行读取两侧源码。Patch 与完整源码均宽容替换非 UTF-8 字节；二进制/NUL 文件仍拒绝文本预览。SVN 不提供 staging、远程写操作或导出。完整源码预览每侧 2MiB，Git 导出每侧 32MiB。日志不得记录源码、diff、token 或绝对路径。

## 19. Definition of Done

一项功能只有同时满足以下条件才算完成：

1. 符合单 Agent + 纯网页架构；
2. 依赖方向没有从 core 指向 adapter；
3. 未安装 Agent 的现有 WebRTC 流程仍成立；
4. 安全校验和错误分类完整；
5. buffer/task/channel 有明确上限；
6. 按用户要求完成相称验证；
7. 实际架构变化同步到本文件、受影响的协议文档，以及网页 `ARCHITECTURE.md`；
8. 没有把临时 benchmark 代码误当生产实现；
9. 最终交付说明修改文件、验证内容、未验证内容和下一阶段门槛。
