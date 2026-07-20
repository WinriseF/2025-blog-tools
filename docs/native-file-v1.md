# WinriseF Native File V1

本文件冻结正式极速文件数据面的协议。测速协议继续由 `protocol-v3.md` 与 `lna-http-v1.md` 描述；正式文件不得复用 benchmark ticket、stripe 或 endpoint。

## 1. 固定版本与常量

| 项目 | 值 |
| --- | ---: |
| LAN Session | 11 |
| Local Bridge | 2 |
| Native File | 1 |
| Bridge/control 最大 JSON | 64KiB |
| LNA HTTP worker | 6 |
| LNA segment | 最大 30MiB |
| I/O block | 4MiB |
| WebTransport connection | 6 |
| 每 connection 单向 lane | 4 |
| WebTransport extent | 64MiB |
| token 硬过期 | 12 小时 |
| transfer 空闲超时 | 120 秒 |

Rust 与 TypeScript 必须共同通过 `protocol-fixtures/native-file-v1.json` 的静态一致性检查。

## 2. 不变量

- WebRTC 是唯一控制面，负责聊天、附件 offer/request/ready、进度、取消和最终确认。
- HTTP/WebTransport 只传文件字节；不得实现第二份聊天或附件状态机。
- 一次会话最多一端运行 Agent；另一端永远是纯网页。
- Agent 持有的源文件字节不得进入安装端网页的 JS heap。
- Agent 全局只有一个 active native transfer，其余大文件由网页按附件顺序排队。
- token 不进入 URL、日志、Supabase 或附件历史；日志不得包含绝对路径和文件内容。
- 首版失败或取消删除 `.part`，不做断点续传、逐块哈希或多文件并行。

## 3. Local Bridge V2

Bridge 路径为 `/winrisef/bridge/v2`。每帧是 `4-byte big-endian JSON length + UTF-8 JSON`，长度必须在 `1..=65536`。

首帧必须是：

```json
{"type":"hello","version":2,"launchToken":"<128-bit hex>"}
```

响应为 `hello-ack`。认证后，命令使用递增 `requestId`；响应统一为：

```json
{"type":"response","requestId":1,"ok":true,"result":{}}
```

命令：

- `select-files`
- `create-send-transfer`
- `prepare-receive-transfer`
- `cancel-transfer`
- `finish-send-transfer`
- `release-source`
- `issue-benchmark-ticket`

文件元数据只包含 opaque source ID、文件名、大小和 MIME。Bridge 可推送 `transfer-progress`、`transfer-confirming`、`transfer-complete`、`transfer-failed`、`transfer-cancelled`；进度最多每 250ms 或每 32MiB 推送一次。

## 4. WebRTC V11 控制消息

`LanBulkDataPlane` 固定为 `webrtc`、`native-lna-http`、`native-webtransport`。`LanAttachment` 和 `attachment-offer` 携带 `dataPlane`；native offer 还携带 source 所在位置和 Agent owner device ID。

正式 native 流程新增：

- `native-transfer-request`：纯网页已准备存储并选择数据面；
- `native-transfer-ready`：Agent 已打开源文件或创建保存目标，并返回 grant。

现有 `attachment-progress`、`attachment-received` 和 `attachment-cancel` 继续使用。`attachment-accept` 与 DataChannel scheduler 只处理 `webrtc` 数据面。

## 5. Grant 与鉴权

grant 只携带网页实际需要的 transfer ID、attachment ID、Agent owner 和授权 token。peer device ID、方向、总大小与数据面由 Agent 内部绑定；分段、并发和过期策略由 Native File V1 固定，不在每个 grant 中重复声明。

- LNA 使用一个 256-bit token，只能放在 `X-WinriseF-Transfer-Token` 请求头。
- WebTransport 使用六个 128-bit token，分别绑定 connection index，且每张只能消费一次。
- token 硬过期 12 小时；transfer 120 秒无活动即失效；完成或取消立即失效。
- 每个 segment/extent 只接受一次；offset、长度、方向、Origin、连接/lane 归属及最终 coverage 必须精确匹配。

## 6. LNA HTTP File API V1

base path 为 `/winrisef/file/v1`：

- `GET /probe`
- `GET /transfers/{id}/segments?offset=&bytes=`
- `POST /transfers/{id}/segments?offset=`
- `POST /transfers/{id}/complete`

六个 worker 在各自 keep-alive 连接上顺序处理 segment。segment index 按 `index % 6` 确定 worker；每段最大 30MiB。浏览器上传必须直接 `xhr.send(file.slice(...))`；下载响应使用 XHR ArrayBuffer，并立即按 4MiB block 写入 StorageEngine。

Agent 每个请求最多租用两个 4MiB buffer。接收 `/complete` 前必须确认六个请求槽全部空闲、总 coverage 精确完成、文件长度一致，随后 sync 并在同一文件系统中原子替换 `.part`。

LNA permission 结果语义不可混淆：

- `denied`：本次极速不可用，禁止转 WebTransport；
- descriptor unsupported：使用正式 WebTransport File V1；
- descriptor supported 但 endpoint 失败：明确报错，禁止伪装为 unsupported。

## 7. WebTransport File V1

路径为 `/winrisef/file/v1`。建立六条独立 connection，每条 connection 在首个双向 control stream 发送 `hello`，声明 version、transfer ID、一次性 token、connection index、方向、peer、4 lanes、4MiB block、64MiB extent 和总大小。

每条 connection 使用四条单向 data stream。每个 lane 的 extent 按全局 lane `connectionIndex * 4 + laneIndex` 确定性轮转；extent header 是 `8-byte offset + 8-byte length` 大端整数，末尾用 `offset = u64::MAX, length = 0` 标记。

每 lane 只持有一个 4MiB buffer，总池上限 24 个、约 96MiB。任一 connection/extent 失败即取消整个 transfer。浏览器上传完成后，由 connection 0 发送 `complete`；Agent 仅在 transfer-wide coverage 完整后 sync、原子完成并返回 `transfer-complete`。

## 8. 文件生命周期

- `select-files` 打开系统多选框，Agent 打开源文件一次并注册 opaque source。
- `prepare-receive-transfer` 打开系统保存框，在目标目录创建随机 `.part`，设置逻辑长度并尽可能预分配。
- 完成接收时 flush/sync，再在同一文件系统内原子替换为最终文件名。
- `finish-send-transfer`、取消、失败、Bridge/Agent 关闭必须释放句柄；未完成接收必须删除 `.part`。
- 不记录完整 token、绝对路径、内容或逐 4MiB block 日志。

## 9. 产品选择规则

正式文件能力不需要额外的部署特性开关。用户开启极速模式且 Agent 连接成功后，普通文件且大小 `>=64MiB` 自动尝试 native；图片、语音、小文件、粘贴和拖放维持 WebRTC。极速模式开启时，安装端回形针调用 Agent 系统文件选择框；关闭时仍使用网页文件选择器。
