# WebRTC IPv6 直连故障调查记录

更新日期：2026-07-22  
涉及仓库：`2025-blog-public`、`2025-blog-tools`  
调查对象：Windows Chrome 与 Android Chrome 通过手机热点建立 WinriseF LAN WebRTC 连接

## 1. 文档目的

本文完整记录 2026-07-22 对“以前可以通过 IPv6 直连、现在即使未开启极速模式也无法连接”问题的排查过程、实验变量、两端日志证据、最终根因和后续修复建议。

原始浏览器诊断日志包含本机网络地址，用户将在结论固化后删除这些日志。本文只保留复现和技术判断需要的信息，不记录房间令牌、邀请链接、密码、ICE 密码、完整 ICE username fragment、Agent 令牌或其他授权凭据。

## 2. 最终结论

本次问题不是单一原因，而是三个条件叠加形成的回归：

1. `2025-blog-public` 的 WebRTC 配置从 Cloudflare + Google 双 STUN 改成了 Google-only。当前热点网络中，Google STUN 只能稳定提供 IPv4 `srflx` 候选，电脑的 IPv6 主机候选仍被 Chrome 隐藏成 `.local` mDNS 地址。
2. Android Chrome 收到了电脑的 IPv6 mDNS 候选，但没有为它建立可用的 IPv6 Candidate Pair。恢复 Cloudflare STUN 后，Cloudflare 能为电脑生成字面 IPv6 `srflx` 候选，Android 随即可以建立 IPv6 Candidate Pair。
3. Windows Defender Firewall 默认阻止进入 Chrome 动态 UDP 端口的连接检查。即使 Cloudflare 已经生成字面 IPv6 候选，删除临时 IPv6 入站规则后，手机发出的 ICE/STUN 检查仍全部被挡住。

因此，本机成功 IPv6 直连需要同时满足：

```text
Cloudflare + Google 双 STUN
        +
允许进入 Chrome 的受限 UDP 防火墙规则
        =
WebRTC IPv6 直连成功
```

双 STUN 只修复“候选发现”；防火墙规则修复“候选可达性”。两者缺一不可。

## 3. 相关代码和提交

WebRTC 配置位置：

```text
E:\Project\PROJECT\2025-blog-public\src\lib\lan-transfer\native-webrtc-transport.ts
```

历史变化：

- `f611a9156f103f60f34aac47dc9066092127e452`，提交信息 `新增IPv6`
  - 配置为 Cloudflare + Google 双 STUN。
- `bd00ef41ceb00c291ea7fa37a1fb8f67720078ad`，提交信息 `fix ipv6`
  - 删除 Cloudflare，只保留 Google STUN。
- `816edccdfc9169663da28d3c6f175cf677dc1ab3`，提交信息 `test dual STUN for IPv6 ICE`
  - 根据本次调查恢复 Cloudflare + Google 双 STUN。
  - 已推送至 `2025-blog-public` 的 `origin/main`。

当前验证配置：

```ts
export const lanRtcConfig: RTCConfiguration = {
	iceServers: [{ urls: 'stun:stun.cloudflare.com:3478' }, { urls: 'stun:stun.l.google.com:19302' }],
	iceCandidatePoolSize: 2,
}
```

Supabase Realtime 在该链路中只负责 Presence/Broadcast 信令。它可以传送 offer、answer 和 Candidate，但不承载 WebRTC 文件数据，也不能改变 Windows 防火墙、mDNS 解析、NDP 或 UDP 可达性。因此 Supabase 不是此次故障的原因，也无法单独修复该问题。

## 4. 测试环境

### 4.1 电脑端

- 操作系统：Windows 10，64 位。
- 浏览器：Chrome 150。
- 网络接口：WLAN，连接到手机热点。
- Windows 网络配置文件：`Public`。
- Windows Firewall 默认动作：入站阻止、出站允许。
- Chrome 原有入站允许规则只有 `Google Chrome (mDNS-In)`：UDP 5353。
- 未发现允许 Chrome 任意动态 WebRTC UDP 端口的通用入站规则。
- Node.js 存在 Public 入站允许规则，因此 OpenSpeedTest 的固定 TCP 监听服务可以工作。

电脑在热点中的主要 IPv6 信息：

- 热点前缀：`2409:8962:431:600::/64`。
- 本机 IPv6 `/128` 地址包括：
  - `2409:8962:431:600:387c:93c9:26df:c370`
  - `2409:8962:431:600:444a:97ab:9ce7:3926`

### 4.2 手机端

- 操作系统：Android 10。
- 浏览器：Chrome 150。
- 同一台手机同时提供热点并运行接收端网页。
- 浏览器公开的热点 IPv4 主机候选：`10.8.138.189`。
- 浏览器公开的 IPv6 主机候选：`2409:8962:431:600:1ce9:a6ff:fe7f:82c2`。

### 4.3 公网/NAT 观察

- 双方观察到的 IPv4 `srflx` 地址为 `39.144.137.116`，表明 IPv4 位于运营商 NAT/热点 NAT 后。
- 成功 IPv6 实验中，手机侧最终出现 Peer Reflexive IPv6：`2409:8962:431:600::81`。这说明热点在实际数据路径中对手机浏览器公开的 IPv6 地址进行了代理、映射或转换。

### 4.4 Windows IPv6 邻居状态

Windows 到手机浏览器公开的 IPv6 主机地址：

```text
2409:8962:431:600:1ce9:a6ff:fe7f:82c2
```

邻居表状态为 `Unreachable`，MAC 为全零。电脑无法通过普通 on-link NDP 直接解析该地址。这解释了为什么“电脑主动向手机的 IPv6 host Candidate 发包”长期得不到响应。

## 5. 为什么 OpenSpeedTest 和显式 IPv6 URL 可以工作

以下地址可以在手机上访问：

```text
http://[2409:8962:431:600:387c:93c9:26df:c370]:3000
```

这与 WebRTC 失败并不矛盾：

- 地址中直接写入了电脑的字面 IPv6，不需要解析 Chrome 的 `.local` mDNS Candidate。
- OpenSpeedTest 使用固定 TCP 3000 端口，不使用 WebRTC ICE 的动态 UDP 端口。
- Windows 上 Node.js 已有 Public 入站允许规则。
- TCP 服务器监听、字面地址访问和 Windows 防火墙规则均与 WebRTC Candidate Gathering/ICE 检查不同。

因此，OpenSpeedTest 成功只能证明：

- 手机可以通过热点访问电脑的某个字面 IPv6；
- TCP 3000 的监听和防火墙规则可用。

它不能证明：

- Android 可以解析电脑的 IPv6 mDNS Candidate；
- Chrome 动态 UDP 端口允许入站；
- WebRTC IPv6 Candidate Pair 可达。

若要完全参考 OpenSpeedTest 的方式，就必须改成“显式 IP + 固定监听端口 + 对应防火墙规则”的本地服务架构。这更接近现有 Agent/LNA/WebTransport 数据面，而不是纯浏览器 WebRTC 的小改动。

## 6. 实验总览

| 实验 | STUN 配置 | Windows Chrome UDP 入站 | 结果 | 核心判断 |
| --- | --- | --- | --- | --- |
| 基线 | Google-only | 默认阻止 | 失败 | 无法区分候选发现和防火墙问题 |
| 显式 IPv6 TCP 3000 | 不涉及 | Node.js 规则允许 | 成功 | IPv6 TCP 固定端口可达 |
| 放开 Chrome UDP，LocalSubnet | Google-only | 允许 | IPv4 NAT 直连成功，约 657/644 Mbps | Windows 防火墙是主要阻断层之一 |
| 仅允许当前 IPv6 `/64` | Google-only | IPv6 入站允许 | 失败 | Android 没有为电脑 IPv6 mDNS 建立 Pair；反向 NDP 也失败 |
| 仅允许当前 IPv6 `/64` | Cloudflare + Google | IPv6 入站允许 | 第二轮 IPv6 直连成功 | Cloudflare 字面 IPv6 `srflx` 绕过 Android mDNS 问题 |
| 删除临时 IPv6 规则 | Cloudflare + Google | 默认阻止 | 失败 | 字面 IPv6 Candidate 已存在，但 Windows 丢弃进入 Chrome 的 UDP |

## 7. 实验一：基线与显式 IPv6 访问

### 7.1 现象

- WinriseF 普通 WebRTC LAN 连接失败，即使没有开启极速模式。
- 同一热点环境下，手机访问电脑字面 IPv6 的 OpenSpeedTest 页面成功。
- 这最初看起来像“IPv6 网络本身可用，但 WebRTC 无法匹配 IPv6”。

### 7.2 初步排除

- Supabase 信令正常，否则双方不会交换到完整的 Candidate。
- 电脑确实拥有 GUA IPv6 地址和 `/64` on-link 路由。
- 显式 TCP 访问成功说明不是所有 IPv6 流量都被热点禁止。
- 问题范围收敛到 WebRTC Candidate 发现、mDNS、动态 UDP、防火墙和热点 NDP。

## 8. 实验二：放开 Chrome UDP 后 IPv4 立即成功

临时放开以下范围的入站 UDP：

- 程序限定 Chrome。
- Profile 为 Public。
- 接口为 WLAN。
- RemoteAddress 为 LocalSubnet。

结果：

- WebRTC 很快建立 IPv4 NAT/Peer Reflexive 直连。
- 上传约 657 Mbps，下载约 644 Mbps。
- 手机端识别为 `IPv4 NAT 直连`。
- 电脑端显示 `未知直连`，原因是 Windows Chrome `getStats()` 隐藏了所选 Candidate 的地址/地址族。

对应电脑日志：

```text
C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-225548.json
```

选中路径大致为：

- 电脑本地 host UDP：端口 `50075`。
- 手机远端 prflx UDP：端口 `32846`。
- Candidate Pair `succeeded`、`nominated`。
- RTT 约 7ms。

该实验确认 Windows 防火墙是普通 WebRTC 连接失败的一个直接原因。但它成功得太快，ICE 优先选择 IPv4，因此不能证明 IPv6 是否可用。

## 9. 实验三：Google-only + IPv6-only 防火墙规则

为了单独测试 IPv6：

- 删除放开 LocalSubnet 的通用测试规则。
- 仅允许来自 `2409:8962:431:600::/64`、进入 Chrome 的 UDP。
- 保持代码为 Google-only STUN。

相关日志：

```text
C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-230715.json
C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-230512.json
```

两端可通过协商编号对应：

- `6eba82ec`
- `49701abe`
- `861110e5`
- `8d0f3447`

### 9.1 电脑端 Candidate

电脑每轮生成两个 mDNS host Candidate 和一个 IPv4 `srflx`：

- 第一 mDNS 端口对应电脑 IPv4。
- 第二 mDNS 端口对应电脑 IPv6。
- Google STUN 只生成 IPv4 `srflx`：`39.144.137.116`。
- 没有字面 IPv6 `srflx`。

以协商 `6eba82ec` 为例：

- mDNS UDP `59349` 对应电脑 IPv4。
- mDNS UDP `59350` 对应电脑 IPv6。
- 电脑向手机 IPv6 `2409:8962:431:600:1ce9:a6ff:fe7f:82c2:51016` 发送 49 次检查。
- `requestsReceived = 0`。
- `responsesReceived = 0`。

后续轮次电脑向手机 IPv6 分别发送 48、31、34 次检查，全部无响应。

这与 Windows 邻居表中手机 IPv6 为 `Unreachable` 一致：电脑主动方向无法通过 NDP 到达手机公开的 host IPv6。

### 9.2 手机端 Candidate Pair

手机每轮都生成：

- IPv4 host：`10.8.138.189`。
- IPv6 host：`2409:8962:431:600:1ce9:a6ff:fe7f:82c2`。
- IPv4 `srflx`：`39.144.137.116`。

手机收到了电脑的两个 `.local` Candidate，但只为电脑的 IPv4 mDNS Candidate 建立 Pair，没有为电脑的 IPv6 mDNS Candidate 建立 Pair。

因此：

- 手机没有向电脑的 IPv6 mDNS Candidate 发送 IPv6 ICE 检查。
- 电脑向手机 IPv6 host 的反方向检查又因 NDP 不可达失败。
- 两个方向都无法建立 IPv6 Pair。

### 9.3 本轮结论

Google-only 下的主要 IPv6 Candidate 问题是：

```text
电脑 IPv6 只以 .local mDNS 形式出现
→ Android Chrome 未建立对应 IPv6 Pair
→ 手机无法主动打通 Windows UDP/NAT/防火墙状态
```

## 10. 实验四：恢复 Cloudflare + Google，保留 IPv6-only 规则

代码恢复双 STUN 后提交并推送：

```text
816edccdfc9169663da28d3c6f175cf677dc1ab3
```

相关日志：

```text
C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-232552.json
C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-232554.json
```

### 10.1 第一轮仍失败

第一轮协商：`7b8ecf15`。

本轮电脑只生成：

- 两个 mDNS host Candidate。
- 两个 IPv4 `srflx` Candidate。
- 没有 Cloudflare IPv6 `srflx`。

ICE 检查持续约 9 至 10 秒，所有 Pair 保持 `in-progress`，没有成功 Pair。日志还出现 Google STUN `701 STUN host lookup received error`。

这说明 Cloudflare 的 IPv6 Candidate Gathering 并非每次第一轮都稳定成功，因此当前自动重建/重试不能删除。

### 10.2 第二轮 IPv6 成功

第二轮协商：`85ec6a96`。

电脑的本地 Candidate：

- IPv4 mDNS host：端口 `60726`。
- IPv6 mDNS host：端口 `60727`。
- Google IPv4 `srflx`：`39.144.137.116:61905`。
- Cloudflare IPv4 `srflx`：`39.144.137.116:20838`。
- Cloudflare IPv6 `srflx`：`2409:8962:431:600:444a:97ab:9ce7:3926:60727`。

关键变化是 Cloudflare 把原本只能作为 `.local` 发送的电脑 IPv6 host Candidate，额外映射成了可通过信令直接传给手机的字面 IPv6 `srflx` Candidate。

手机收到电脑的字面 IPv6 后建立成功 Pair：

- 手机本地 prflx IPv6：`2409:8962:431:600::81:44995`。
- 其 related address 为手机浏览器公开的 host IPv6：`2409:8962:431:600:1ce9:a6ff:fe7f:82c2:44995`。
- 电脑远端 `srflx` IPv6：`2409:8962:431:600:444a:97ab:9ce7:3926:60727`。
- Candidate Pair 状态：`succeeded`。
- RTT：约 8ms。
- 手机端路线判断：`family=ipv6`、`kind=direct`。

电脑端同一 Pair：

- 本地 host UDP 端口：`60727`。
- 远端 prflx UDP 端口：`44995`。
- `requestsSent = 1`。
- `requestsReceived = 2`。
- `responsesSent = 2`。
- `responsesReceived = 1`。
- RTT：约 6ms。
- Pair 为 `succeeded`、`nominated`、`writable`。

电脑端 `getStats()` 隐藏了所选 host/prflx 的完整地址，因此 `selected-route` 显示 `unknown/unknown`。但两端端口、协商编号、流量计数和手机端明确的 IPv6 Pair 完全一致，可以确认电脑端的“未知直连”实际就是同一条 IPv6 直连。

连接建立后直到导出日志未出现 disconnected/failed 状态。

### 10.3 关于 701 错误

成功协商中仍出现：

- Google：`701 STUN host lookup received error`。
- Cloudflare：稍后出现 `701 STUN binding request timed out`。

这些错误可以对应某一个 DNS 结果、地址族、网络接口或 STUN 事务，不能直接解释成整个 STUN 服务失败。因为同一轮日志已经明确记录 Cloudflare 成功生成 IPv4 和 IPv6 `srflx`，且 IPv6 Pair 已实际连接。

### 10.4 本轮结论

本轮直接证明：

```text
Cloudflare IPv6 srflx
→ Android 获得字面电脑 IPv6
→ Android 创建 IPv6 Pair 并主动发送检查
→ Windows IPv6-only 规则放行
→ 手机出现 prflx IPv6
→ IPv6 直连成功
```

这也解释了“以前可以 IPv6”的一种确定机制：历史双 STUN 配置能够提供字面 IPv6 Candidate。以前 Cloudflare 也曾失败，并不能否定该机制，因为当时 Chrome UDP 入站仍可能被 Windows 防火墙阻断。只有“双 STUN + 防火墙规则”组合实验才能验证完整链路，而该组合已经成功。

## 11. 实验五：保留双 STUN，删除 IPv6 防火墙规则

为了判断代码修改是否足够，在保持双 STUN 的情况下删除临时 IPv6 入站规则。

相关日志：

```text
C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-233126.json
C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-233206.json
```

双方共同出现并失败的协商包括：

- `eba0be73`
- `b4001088`
- `4b70699c`
- `adc9e488`

没有任何协商进入 connected。

### 11.1 最关键协商 `eba0be73`

本轮 Cloudflare 正常生成电脑字面 IPv6 Candidate：

```text
2409:8962:431:600:444a:97ab:9ce7:3926:55555
```

手机收到并建立 IPv6 Pair：

- 手机 host IPv6：`2409:8962:431:600:1ce9:a6ff:fe7f:82c2:53103`。
- 电脑 Cloudflare `srflx` IPv6：端口 `55555`。
- 手机发送 35 次检查。
- 手机 `responsesReceived = 0`。

电脑侧对应 Pair：

- 电脑 host UDP：端口 `55555`。
- 手机 host IPv6：端口 `53103`。
- 电脑发送 39 次检查。
- 电脑 `requestsReceived = 0`。
- 电脑 `responsesReceived = 0`。

这次已经不存在“Android 没有 IPv6 Pair”问题。手机明确建立并使用了 IPv6 Pair，但进入电脑 Chrome 的 UDP 没有被浏览器收到。

### 11.2 IPv4 旁证

同轮手机 IPv4 Pair 记录：

- 手机收到电脑发来的约 35 次检查。
- 手机发送约 35 次响应。
- 电脑对应 Pair 的 `responsesReceived` 仍为 0。

这表明手机确实回应了电脑，响应却没有到达 Chrome 的 WebRTC Socket。结合删除规则这一唯一变量，Windows Firewall 入站阻断是最符合证据的解释。

### 11.3 其他重试

- `b4001088` 和 `adc9e488` 很快进入 disconnected，未形成有效 Pair。
- `4b70699c` 中电脑没有获得字面 IPv6 Candidate；手机虽然有 IPv6 Candidate，但没有电脑 IPv6远端 Candidate，最终同样失败。
- 多轮重试说明双 STUN 提高了 Candidate 能力，但不能绕过 Windows 防火墙，也不能保证 Cloudflare 每轮都生成 IPv6 Candidate。

### 11.4 本轮结论

删除规则后失败，证明双 STUN 不是完整的端到端修复：

```text
Candidate 已发现
→ Candidate Pair 已创建
→ 手机检查已发出
→ Windows Firewall 阻止 UDP 进入 Chrome
→ Pair 永远无法 succeeded
```

## 12. 根因分层

### 12.1 已确认：STUN 配置回归

Google-only 在当前网络下不能为电脑提供可用的字面 IPv6 `srflx`。Cloudflare 可以提供，且提供后 Android 成功创建并使用 IPv6 Pair。

处理：保留 Cloudflare + Google 双 STUN。

### 12.2 已确认：Windows Chrome UDP 入站被阻止

没有受限入站规则时，手机发出的 IPv4/IPv6 ICE 响应和检查都无法到达 Chrome。添加规则后同样环境立即可以连接。

处理：本机需要受限 Chrome UDP 规则；产品上需要手动引导、TURN 或 Agent 数据面。

### 12.3 已确认：Android/Chrome 对电脑 IPv6 mDNS 路径不可用

Google-only 日志中，Android 收到电脑 IPv6 `.local` Candidate，但没有创建相应 IPv6 Pair。Cloudflare 字面 IPv6 绕过后立即成功。

处理：不能只依赖 host mDNS Candidate；必须保留可产生字面 IPv6的 STUN 或采用显式 Agent 端点。

### 12.4 已确认：热点 IPv6 具有非普通 on-link 行为

Windows 对手机公开 host IPv6 的 NDP 状态为 `Unreachable`；成功时手机实际以 `2409:8962:431:600::81` prflx 出现在 Pair 中，而不是直接使用浏览器公开的 `...:82c2`。热点内部存在代理、地址转换或特殊转发行为。

处理：不要假设同一 `/64` 的浏览器 host Candidate 一定可以通过普通 NDP 双向到达，应让 ICE/STUN 发现 Peer Reflexive 路径。

### 12.5 已确认：电脑端“未知直连”是观测问题

Windows Chrome 对所选 host/prflx Candidate 隐藏地址，导致 `inspectRoute()` 无法判断地址族。手机端能够明确识别 IPv6，且端口和 Pair id 可以对应。

处理：可以在 DataChannel 建立后交换双方各自观察到的结构化 route family/kind；不要把完整地址写入 UI 或长期状态。

## 13. 当前状态

截至本文更新：

- `2025-blog-public` 主分支已经包含双 STUN 提交 `816edccdfc9169663da28d3c6f175cf677dc1ab3`。
- 双 STUN 配置经过实际 IPv6 直连验证，应该保留。
- 最后一轮实验删除了临时 IPv6 防火墙规则；因此当前电脑在不重新添加规则的情况下，预计仍无法建立普通 WebRTC 直连。
- 当前重试机制在首轮未生成 IPv6 Candidate 时成功触发第二轮并恢复连接，不能删除。
- 当前高频 ICE Pair 诊断对定位首轮/第二轮差异仍有价值，暂不建议立即清理。

## 14. 短期修复建议

### 14.1 当前电脑

重新建立受限的 Windows Firewall 规则：

- Direction：Inbound。
- Action：Allow。
- Program：Chrome 可执行文件。
- Protocol：UDP。
- Profile：Public。
- Interface：WLAN。
- RemoteAddress：当前热点 IPv6 `/64`，或者经过安全评估后使用 LocalSubnet。

使用固定 `/64` 暴露范围较窄，但运营商重新分配热点 IPv6 前缀后规则需要更新。使用 LocalSubnet 能适应前缀变化，但同时可能允许同一 WLAN 内的 IPv4/IPv6设备访问 Chrome 动态 UDP端口。

不建议开放任意程序、任意来源或所有接口的入站 UDP。

### 14.2 WebRTC 配置

- 保留 Cloudflare + Google 双 STUN。
- 不强制某一地址族，不在业务层过滤 IPv4/IPv6 Candidate。
- 保留连接失败后的 ICE Restart/Transport Rebuild。
- 不因为某个 STUN `701` 就立即取消其他已成功收集的 Candidate。

## 15. 产品级长期方案

纯网页无法自行添加 Windows Firewall 规则，因此要区分“直接修好当前电脑”和“让所有普通网页用户可靠连接”。

### 15.1 普通网页模式

建议顺序：

1. WebRTC host/srflx 直连优先。
2. 连接诊断识别“有 Candidate 但所有响应为 0”的情况。
3. 给 Windows 用户显示明确、受限的防火墙修复说明。
4. 若产品要求无需本地程序和手工规则也必须连接，则部署 TURN 作为最终兜底。

TURN 能提高可用性，但文件数据经过中继，带来带宽、费用和吞吐限制。Supabase 和 EdgeOne 页面部署本身不会自动提供 TURN。

### 15.2 极速模式/Agent

Agent 可以：

- 绑定自身拥有的固定端口。
- 为自己的可执行文件申请受限防火墙规则。
- 发布明确 IPv4/IPv6端点。
- 采用 LNA HTTP、WebTransport/QUIC 或其他可控数据面。

这最接近 OpenSpeedTest 的“显式地址 + 固定监听端口”模型，也是大文件高速直连更可靠的路径。但它依赖本地 Agent，不能替代普通纯网页模式的无安装体验。

## 16. 建议追加验证

在宣布完全稳定前，建议保留详细日志并进行以下验证：

1. 恢复受限 IPv6防火墙规则后，关闭旧标签页，连续冷启动配对至少 5 次。
2. 记录每轮 Cloudflare 是否生成 IPv6 `srflx`，以及第几轮建立连接。
3. 验证热点断开重连、IPv6前缀变化后规则是否仍适用。
4. 验证另一台 Android/Windows设备，区分设备特有问题和通用 Chrome行为。
5. 验证连接建立后消息、小文件和长时间传输，不只验证 ICE connected。
6. 验证自动重建后附件调度和历史恢复状态是否正确。
7. 若未来部署 TURN，验证 direct 优先且只有 direct 失败时才使用 relay。

成功标准至少包括：

- 手机或电脑一端明确记录 `family=ipv6`、`kind=direct`。
- 对应 Candidate Pair 为 `succeeded`，并被 Transport 选中。
- `requestsReceived`/`responsesReceived` 不再全部为 0。
- 数据通道建立并可双向传输。
- 连接在诊断窗口内无 disconnected/failed。

## 17. 诊断日志索引

以下文件用于本次结论。用户可能在本文保存后删除它们。

| 文件 | 用途 |
| --- | --- |
| `C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-225548.json` | 放开 Chrome LocalSubnet UDP 后的 IPv4 NAT成功和测速证据 |
| `C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-230047.json` | IPv6-only 早期失败导出，信息较少 |
| `C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-230715.json` | Google-only + IPv6-only 电脑端完整失败证据 |
| `C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-230512.json` | Google-only + IPv6-only 手机端失败证据 |
| `C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-232552.json` | 双 STUN + IPv6规则电脑端成功证据 |
| `C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-232554.json` | 双 STUN + IPv6规则手机端成功证据 |
| `C:\Users\Flynn\Desktop\winrisef-web-diagnostics-20260722-233126.json` | 删除防火墙规则后电脑端失败证据 |
| `C:\Users\Flynn\Downloads\winrisef-web-diagnostics-20260722-233206.json` | 删除防火墙规则后手机端失败证据 |

## 18. 一句话结论

此次 IPv6 回归的完整原因是：Google-only 使电脑缺少 Android 可用的字面 IPv6 Candidate，而 Windows Firewall 又阻止进入 Chrome 动态 UDP 端口的 ICE 检查；恢复 Cloudflare + Google 双 STUN并添加受限 Chrome UDP 入站规则后，日志已实证 WebRTC 通过 IPv6 `srflx ↔ prflx` Candidate Pair 成功直连。
