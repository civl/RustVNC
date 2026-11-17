# M0 兼容性报告 v1

> 日期：2026-11-17 · 环境：macOS 27.2.1（Mac mini M4，Apple Silicon）/ Tailscale
> 目标：证明“能连上、能出画、能输入”，记录 macOS Screen Sharing 的协议差异。
> 真实会话数据同时记录在 [BLOCKER-ANALYSIS.md](BLOCKER-ANALYSIS.md) 和 [ACCEPTANCE.md](ACCEPTANCE.md)。

## 1. 结论

| 项 | 结果 |
|---|---|
| RFB 握手 / 版本串 | ✅ 实测 macOS 声明 `RFB 003.889`，客户端回 `003.008` |
| 安全类型探测 | ✅ `[30, 33, 36, 31, 32, 2, 35]`；ARD 与 VNC 密码同时可用 |
| Apple ARD 账号认证 | ✅ 真机成功（无需 VNC 密码） |
| VNC 密码认证失败路径 | ✅ 错误密码返回 `Wrong password` |
| 5K 出画 | ✅ 5120×2880，8 秒 459 帧，57.31 fps |
| 1080p soak | ✅ 90 秒 5,100 帧，56.65 fps，0 错误 / 0 断线 |
| ZRLE | ✅ 主力编码 |
| zlib(6) | ✅ 真机可用；5 秒解出 2 个 5K 全屏帧，像素正确 |
| Tight(7) | ❌ macOS 不认，回退 Raw；默认不请求 |
| 16bpp | ✅ 1080p，61–62 fps，颜色正确 |
| Raw | ✅ 1080p 约 10 fps |
| Apple 私有编码 | ✅ 1107/1109/1110 真机各一次；1109=ABC 布局、1110=Mac16,10；1104 指针不动时不发，1105 单屏未发 |
| 未知编码 | ✅ 明确报编码号、不可重试；不再每 1 秒重连 |
| 键鼠输入 | ✅ mock RFB Pointer/Key 端到端；真机输入注入仍待人工 |
| 剪贴板 | ✅ RFB 5900 不承载；由 Agent 通道验证双向 |
| 复用结论 | ✅ `vendor/vnc-rs 0.6.1` + 补丁；暂不需要长期 fork |

**判定：M0 协议可行性通过。** 10 分钟连续出画与真机键鼠注入属于验收项，列入 M1/ACCEPTANCE，不再是未知协议风险。

## 2. 关键实测

### 2.1 握手与认证

```bash
cargo run -p rfb-probe -- --inspect --host 127.0.0.1 --port 5900
```

```text
server_version: RFB 003.889
security_types: [30, 33, 36, 31, 32, 2, 35]
```

ARD 成功由真实会话验证；错误凭据路径仍可用：

```text
Apple Remote Desktop authentication failed: Authentication or authorization failure
Wrong password
```

### 2.2 编码

- 主力：ZRLE；zlib(6) 可用；CopyRect/Raw 正常。
- 16bpp：正常，且修复了 `u8 * 255` 溢出导致的近黑画面。
- Tight：macOS 不认；保留为可选，不默认请求。
- Apple 私有编码：解码器按 `u16` 长度安全读取；1109/1110 已解析并写入诊断日志；
  1104 解析矩形 hotspot/size；1105 用候选布局校验后记录 pixel size/scale/display id（未驱动视口）。
- 未知编码：`UnsupportedEncoding(n)`，不可重试，避免重连风暴。

### 2.3 性能

| 分辨率 | 结果 |
|---|---|
| 5120×2880 | 57.31 fps / 8 s；修复 `MAX_PIXELS` 4K 上限后通过 |
| 1920×1080 | 56.65 fps / 90 s，0 错误 |
| 1920×1080 @16bpp | 61–62 fps |

`STATS.bytes` 是解码后字节数，不是网络流量；带宽需抓包或系统计数器。

## 3. 输入与 mock

`crates/rfb-probe/tests/mock_vnc.rs` 覆盖：

- RFB 3.8 VNC DES 认证
- Raw 帧 + PNG
- Pointer/Key 事件
- 自动退出

真机键鼠注入、输入延迟 P95、登录窗口输入按 [ACCEPTANCE.md](ACCEPTANCE.md) 第 3/7 节执行。

## 4. 仍未覆盖

| 项 | 状态 / 计划 |
|---|---|
| 真机键鼠注入与 P95 | 需人工按 ACCEPTANCE 执行；避免干扰正在使用的 Mac |
| 10 分钟连续出画 | 已有 90 秒 soak；10 分钟待补 |
| Apple 1105 字段顺序 | 候选布局已做校验与日志，但真实 1105 字节未抓到（单屏不出）；需移动指针/多屏抓包确认 |

### 抓取 1104/1105 原始体

本机有 BetterDisplay 虚拟屏时，用真实会话抓一次：

```bash
./scripts/capture-apple.sh
```

脚本会隐藏输入 macOS 密码，只把 `rfb-probe --apple` 的
`apple encoding <code>: ... body=... [hex]` 输出写入
`target/apple-capture/*.txt`，不写密码。拿到 1105 的十六进制后即可确认字段顺序。
| 多 macOS 版本矩阵 | 当前仅 macOS 27.2 / Apple Silicon；14/15/26、Intel 未测 |
| Tight/JPEG 质量旋钮 | macOS 不支持，归档为已知差异 |
| Windows 真机输入/延迟 | 需 Windows 真机 + 凭据 |

## 5. 复现环境

| 项 | 值 |
|---|---|
| OS | macOS 27.2.1 |
| CPU | Apple Silicon（Mac mini M4） |
| Screen Sharing | 已启用；ARD 账号认证 |
| Rust | 1.100.0 |
| 协议引擎 | vendored vnc-rs 0.6.1 + `PATCHES.md` |
| 探针 | `rfb-probe` |
