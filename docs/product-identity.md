# WinriseF Toolbox：产品标识与版本

## 产品标识

- 产品名：**WinriseF Toolbox**
- Windows 本机组件：**WinriseF Toolbox Agent**
- 中文说明：**WinriseF 工具箱本机连接器**
- 技术工程名：`WinriseF Native Transfer`
- 可执行文件：`WinriseF-Toolbox-Agent.exe`
- 内部二进制名：`winrisef-agent`
- 协议 Scheme：`winrisef://`

Agent 是 Toolbox 与电脑之间的便携本机能力接口。当前提供局域网互传的极速模式；未来可在不改变产品名的前提下增加文件、设备、剪贴板或其他本机能力。

## 发布版本

当前发布版本为 **v0.1.0-beta.1**。

- Cargo package version：`0.1.0-beta.1`
- Windows fixed file/product version：`0.1.0.1`
- Windows display version：`0.1.0 Beta 1`

发布版本与通信协议版本独立维护：LAN Session 为 V11、Local Bridge 为 V2、Native File 为 V1、测速协议为 V3。协议不兼容时只提升相应协议 major，不以 EXE 的显示版本推断兼容性。
