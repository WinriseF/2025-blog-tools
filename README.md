# WinriseF Native Transfer

WinriseF Native Transfer 是与现有 `2025-blog-public` 协同工作的无前端 Rust 文件传输加速器。

## 永久产品边界

- 一次会话永远最多只有一端安装并启用 Rust Agent。
- 即使两端都是电脑，也禁止 Agent ↔ Agent；另一端始终使用现有网页。
- 两端都是手机时只使用现有网页/WebRTC，Rust Agent 不参与。
- 本仓库不提供网页、WebView、桌面窗口或独立聊天界面。
- Agent 以单个便携 EXE 发布，不使用安装器、后台服务、托盘、开机启动或文件复制；首次双击时只为当前 Windows 用户注册自身所在路径。
- 唯一目标是让安装端原生文件 I/O 与远端网页之间获得尽可能高的持续吞吐。
- 断点续传、多文件并发和历史等高级能力只有在基准证明不损害热路径后才会加入。

详细阶段见 [plan.md](plan.md)，架构和开发规则见 [agent.md](agent.md)。

## 当前可测试状态

当前已经形成“单 Agent + 远端纯网页”的双向内存测速闭环：

- `winrisef-core`：固定二进制协议、64MiB extent 调度、覆盖检查、4MiB buffer pool；
- `winrisef-agent`：WebTransport/HTTP3 服务端、13 天以内 P-256 临时证书、Origin/token 鉴权、双向内存源/汇、四条单向数据流和每秒一次本地指标；
- `winrisef://`：Windows 当前用户协议注册、网页启动回调和一次性 launch token；
- Local Bridge：安装端网页只负责签发短期 peer ticket，不承载测速 payload；
- 远端网页：通过现有 WebRTC 获取一次性 ticket 后，直接连接 Agent 做 browser→Agent 和 Agent→browser 测速。

正式 TypeScript adapter 位于：

```text
E:\Project\PROJECT\2025-blog-public
```

## 固定热路径参数

```text
WebTransport sessions = 有界，默认 1
data lanes            = 4
extent size           = 64 MiB
I/O block             = 4 MiB
buffers per lane      = 2
Agent buffer pool     = 32 MiB
```

网络流内不使用 JSON、逐块哈希、逐块 ACK 或逐块日志。协议见 [docs/protocol-v1.md](docs/protocol-v1.md)。

## 首次使用与手工测试

正式用户只需要下载单个 `winrisef-agent.exe` 并双击一次。无参数启动会：

1. 将当前 EXE 路径注册为当前用户的 `winrisef://` handler，不要求管理员权限；
2. 打开 `https://e.winrisef.top/t?agent-ready=1`，由网页提示“极速组件已准备完成”；
3. 立即退出，不驻留、不创建服务，也不加入开机启动。

以后用户只需打开网页并开启“极速模式”，浏览器便会按需唤起 Agent。如果 EXE 被移动或重命名，再双击一次新位置的 EXE 即可覆盖并修复注册路径。删除 EXE 前可通过开发命令撤销注册；正式下载页后续应提供同样的卸载说明。

开发者构建后的首次流程可用下面的命令模拟双击；普通用户不需要输入命令：

```powershell
cargo build -p winrisef-agent
.\target\debug\winrisef-agent.exe
```

开发版 Agent 默认信任正式站点 `https://e.winrisef.top` 与本机 loopback Origin。若两台设备通过其他 HTTPS 测试 Origin（例如局域网开发证书或预览域名）打开网页，注册时必须把这个**精确 Origin**写入 handler，且不要带末尾 `/`：

```powershell
.\target\debug\winrisef-agent.exe register-protocol --trusted-origin https://192.168.1.10:3000
```

然后：

1. 首次建立远端高速连接时，按 Windows 提示允许 `winrisef-agent.exe` 使用所需网络；
2. 两台设备使用同一个、且已被 Agent 信任的 HTTPS 部署 Origin 打开 `2025-blog-public` 的 `/t`；
3. 安装端电脑创建局域网会话，在“保持屏幕常亮”下开启“极速模式”，并允许浏览器打开 `winrisef://`；
4. Agent 会打开一个短期回调页，原传输页随后显示“本机组件已连接”；
5. 另一台不安装程序的设备扫码加入，等待“极速模式”显示“已发现加速电脑”；
6. 选择 `64MB`、`256MB` 或 `1GB`，点击“测上传”或“测下载”。

Agent 与已认证的本机 Bridge 同生命周期：关闭传输页或关闭极速模式后会释放监听端口；启动回调在约两分钟内未完成时也会自动退出，因此可以直接重复下一轮测试。

### 临时完整诊断日志

当前处于首次浏览器互操作排障阶段，Rust Agent 的日志临时固定为全局 `trace`。`launch`、`serve`、协议注册和协议移除每次运行都会创建独立日志文件，并同时输出到控制台：

```text
C:\Users\Flynn\Documents\WinriseF-Agent-Logs\winrisef-agent-<时间>-<PID>.log
```

复现 `Opening handshake failed` 后，按修改时间选择最新日志。日志覆盖进程启动/退出、短期证书元数据、UDP socket、Quinn transport、TLS、HTTP/3 SETTINGS、WebTransport CONNECT、Origin/path 路由、Bridge、ticket 和内存传输状态机。日志禁止记录证书私钥、完整 launch token、完整 peer ticket、文件内容和热路径 payload；不会为每个数据块输出日志。问题定位完成后必须将全局 `trace` 降级并清理这套临时诊断输出。

本地 `http://localhost` 只适合验证安装端 Bridge。跨设备 WebTransport 测速必须使用两端完全相同的 HTTPS Origin；通过局域网 IP 打开的普通 HTTP 页面不是安全上下文，不能使用 WebTransport。

如果要撤销开发版协议注册：

```powershell
.\target\debug\winrisef-agent.exe unregister-protocol
```

当前闭环是内存源/汇性能测试，不会打开真实文件。系统文件选择、NVMe positional I/O 和正式附件数据面属于后续文件 I/O 阶段。

## 手工 Serve 参数契约

需要绕过自定义协议进行底层调试时，使用 `winrisef-agent serve` 并显式指定：

- `--listen`：WebTransport UDP 监听地址；
- `--allowed-origin`：允许连接的现有网页 HTTPS Origin，可重复；
- `--path`：WebTransport URL path；
- `--token`：32 个十六进制字符的临时 Phase-0 token；
- `--max-transfer-size`：内存基准允许的最大声明大小；
- `--max-sessions`：有界并发 session 数，范围 1–8。
- `--metrics`：每秒采样 CPU/RSS；默认关闭，避免干扰极限吞吐测试。

`serve` 启动时 Agent 只向本地日志输出监听端口和临时证书 SHA-256，不输出 token。正常网页流程使用 `launch`，launch token 只进入 HTTPS callback fragment，peer ticket 通过已加密的 WebRTC DataChannel 交付且只能使用一次。

## 验证纪律

“只验证不运行”时允许：

```text
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
```

禁止启动 Agent、执行传输、运行 benchmark 或浏览器自动化。
