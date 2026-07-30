# WinriseF LNA HTTP Memory Benchmark API v1

状态：私有 IPv4/ULA 路径上的默认极速测速数据面

传输：明文 HTTP/1.1 over TCP（局域网）

正式 Origin：`https://e.winrisef.top`
Base path：`/winrisef/lna/v1`

## 1. 能力与回退

远端纯网页必须在用户动作下查询 `navigator.permissions.query({ name: "local-network-access" })`：

- `denied`：不得请求私有 HTTP/ULA LNA endpoint；若 Agent 已发布并授权公网 IPv6 WebTransport endpoint，可以走该独立路径，否则保留普通 WebRTC；
- permission descriptor 不受支持：使用 `protocol-v3.md` 的六上/六下 WebTransport；
- `prompt` 或 `granted`：请求 `GET /probe`，成功后才进入 HTTP/TCP 测速；
- LNA 受支持但 probe 失败：报告 Agent/防火墙/CORS 错误，不伪装成浏览器不支持。

## 2. HTTP endpoints

### `GET /probe`

不需要 ticket，只允许精确受信 Origin，成功返回 `204`。它只用于触发/验证 LNA 权限，不暴露设备信息。

### `POST /benchmark`

browser→Agent memory benchmark。请求必须带：

- `Origin: https://e.winrisef.top`；
- `X-WinriseF-Ticket: <32 hex>`；
- 确切 `Content-Length`；
- 不允许 `Transfer-Encoding`。

Agent 流式读取并丢弃 body，成功返回实际字节数和 Agent 请求耗时 JSON。

### `GET /benchmark?bytes=<n>`

Agent→browser memory benchmark。请求使用同样的一次性 ticket header。Agent 返回精确 `Content-Length` 的零填充二进制 body。

### `OPTIONS`

已知 path 返回严格 CORS/LNA preflight：允许 `GET, POST, OPTIONS` 与 `Content-Type, X-WinriseF-Ticket`，包含 `Access-Control-Allow-Private-Network: true`，缓存十分钟。

## 3. 并发与有界性

- 网页固定使用六个并发 XHR worker；
- 逻辑总量先均分给六个 worker，每个 worker 再拆成不超过 30MiB 的顺序请求；
- 每个 HTTP 请求消费一张不同的 120 秒一次性 ticket；1GiB 测速需要 36 张 ticket；
- 浏览器最多同时保留六个约 30MiB 响应，不构造或缓存 1GiB 单体 Blob；
- Agent 每 active 请求只使用 1MiB 应用层 I/O buffer，所有下载共享一个 1MiB 零块；
- Agent 单请求硬上限为 64MiB，active 数据请求默认最多六个。

## 4. 安全边界

- LNA 授权由 Chrome 管理，用户拒绝后不得以私有 HTTP/ULA 或伪装 permission 状态绕过；已授权的公网 IPv6 WebTransport 是独立数据面，不依赖 LNA；
- Agent 只接受精确 Origin、固定 path/method、单一 Content-Length 和合法一次性 ticket；
- token 不进入 URL、日志、Supabase 或 capability advertisement；
- HTTP/TCP payload 在局域网上不加密；ticket 提供授权和防重放，但不提供机密性。这是当前以吞吐优先的明确产品取舍；
- Rust endpoint 只是数据 API，不提供 HTML、静态资源、管理页面或任何前端。
