# RustVNC

[![CI](https://github.com/civl/RustVNC/actions/workflows/ci.yml/badge.svg)](https://github.com/civl/RustVNC/actions/workflows/ci.yml)

用 Rust 构建、面向 Windows 用户的 **macOS 远程桌面**客户端。不做通用 VNC 浏览器：只把
"从 Windows 用 Mac"这一件事做到原生手感——高分辨率出画、低延迟输入、剪贴板、音频、
安装包与自动更新。

```text
Windows 客户端 ── RFB/SSH (5900) ──────────► macOS screensharingd   画面 + 输入
               └─ QUIC / TLS 1.3 (8443) ───► RustVNC Agent         剪贴板 + 音频 + 控制
```

两条链路刻意解耦：

- **画面与输入走标准 RFB**，Mac 上零安装，兼容 Apple 原生屏幕共享；
- **增强功能走 Agent**（QUIC + mTLS）：剪贴板双向、Opus 系统音频、远端分辨率控制。
  Agent 挂了只丢这些功能，画面会话不受影响。

## 特性

| 分类 | 能力 |
|---|---|
| **画面** | ZRLE / zlib / CopyRect / Raw；16/32bpp；Apple 私有编码 1104/1107/1109/1110 解析；未知编码明确报错而非静默错乱 |
| **传输** | 单请求在途 + 整帧呈现；自适应请求节奏（延迟 EMA + 双阈值滞回）；静止自动降频 |
| **输入** | 完整修饰键状态机、Windows→macOS 修饰键映射（可配）、特殊键菜单、远端光标形状（32bpp） |
| **视图** | 适应窗口 / 1:1 像素 / 50% / 100% / 200%，超出视口双向滚动；HiDPI 按 `pixels_per_point` 换算；全屏（F11）；**区域裁切**（整屏/左右上下半/自定义，多显示器合并画面按屏取景） |
| **会话** | 多标签（后台标签暂停请求、切回 `FullRefresh`）、重启还原标签与窗口几何、自动重连（指数退避）、连接/握手超时 |
| **系统** | 单实例互斥（按用户，跨登录会话；再次启动会唤起已有窗口）、托盘常驻（关窗隐藏）、系统通知（断线/重连/新版本，窗口在前台时不弹）、开机自启（`--minimized` 静默启动）、凭据存 Credential Manager、诊断日志与诊断包 |
| **安装** | Inno Setup 安装包 + WiX per-user MSI；Authenticode 签名脚本；manifest 更新（SHA-256 校验 + 回滚） |
| **Agent** | 8 位配对码（300 秒一次性，带尝试次数与按 IP 限流）、mTLS 证书固定、剪贴板、Opus 48 kHz 立体声、**自动降分辨率**（按客户端屏幕选目标，帧率 <30 时逐级下退） |

## 进度

> 里程碑定义见 [design.md](design.md)，逐项细节见 M0/M1/M2 三份报告。

| 里程碑 | 内容 | 状态 |
|---|---|---|
| **M0** | 协议兼容性（握手 / ARD 认证 / 编码 / Apple 伪编码） | ✅ |
| **A0–A2b** | macOS Agent：骨架、QUIC 控制面、菜单栏与剪贴板、音频 | ✅ |
| **M1** | Windows 客户端会话核心：出画、输入、重连、诊断 | ✅ |
| **M2a** | 渲染 / DPI / 视图比例 / 区域裁切 | ✅ 比例、HiDPI、区域取景、分辨率自适应；按显示器逐屏流未做 |
| **M2b** | 自适应请求控制器（延迟 EMA + 滞回 + 静止降频） | ✅ |
| **M2c** | 多标签 / 托盘 / 通知 / 开机自启 | ✅ 含单实例互斥、工作区跨重启、系统通知 |
| **M2d** | MSI / Authenticode / 自动更新 / 回滚 | 🟡 真实证书与更新服务器待补 |
| **M2e** | 性能、内存、8 小时 soak | 🟡 工具已就绪 + 1440p 实测 59.9 fps；8 小时 soak 与 4K60 基准待跑 |

## 快速开始

### 1. 构建

```powershell
cargo build --release -p rustvnc-client
.\target\release\rustvnc-client.exe
```

> **请用 release 运行**。5K 画面下 debug 构建的解码慢到肉眼可见（拖窗口像在扫屏）。

要求：**Windows 11**（主支持；Windows 10 22H2 尽力兼容），x64 或 arm64；Rust 1.88+（edition 2024）。
Mac 侧需 **macOS 14+** 并开启屏幕共享。显卡优先 D3D12，老机器依次回退到 Vulkan / OpenGL——
见下方「关键设计取舍」。

### 2. 首次连接

1. Mac 上开启 **系统设置 → 通用 → 共享 → 屏幕共享**；
2. 客户端"新建"：填主机、端口 5900，认证方式选"自动（优先 ARD）"；
3. 账号密码存入 Windows 凭据管理器（不写配置文件）。

### 3. SSH 隧道（可选）

把 5900 关到公网之外。密码认证用 `SSH_ASKPASS`（客户端自身以 `--ssh-askpass` 模式运行）：
Unix 需 OpenSSH 8.4+、Windows 需 8.6+；更早的版本会明确提示改用密钥，并提供一键生成
ed25519 密钥 + `authorized_keys` 指引。host key 首次连接需显式信任（TOFU），更换会拒绝。

### 4. Agent（可选，解锁增强功能）

```bash
# 在 Mac 上
./scripts/build-app.sh release && ./scripts/install-app.sh
```

菜单栏打开 RustVNC Agent，把菜单栏显示的 **8 位配对码** 填进 Windows 客户端——配对码 300 秒内
一次性有效（错误次数有上限与限流）。之后剪贴板与音频自动生效；远端分辨率需额外在 Mac 菜单栏
开启"允许远程调整分辨率"。

## 工程质量

| 项目 | 现状 |
|---|---|
| 测试 | Windows 侧 **175 个**（含 mock RFB 端到端、QUIC 回环、像素级比对、输入延迟基准、协议属性测试）；macOS 侧由 CI 覆盖 |
| 质量门 | `cargo fmt --check`、`clippy -D warnings`、**debug 与 release 双模式测试**、actionlint |
| 本地非 Windows 检查 | `pwsh -File scripts/check-nonwindows.ps1`：Windows 开发机只能编 Windows cfg，这个脚本把 `#[cfg(windows)]` 反转为假、`not(windows)` 反转为真，对自包含模块跑 `clippy -D warnings`，抓 macOS CI 才会暴露的 dead code（本项目踩过两次） |
| 一键验收 | `powershell -File scripts/acceptance.ps1`（8 步，含真机探针） |
| 性能预算 | `scripts/bench-soak.ps1`（CPU/内存/句柄 + 稳态回归 + 阈值判定）、`--bench-frames`（本地呈现）、`tests/input_latency.rs`（输入→像素 P95） |
| CI | 5 个 job：Ubuntu 26.04（workflow lint / cargo-deny）、macOS 26、Windows Server 2025 ×2 |
| 产物 | `RustVNC-Setup-*.exe`（Inno）、`RustVNC.msi`（WiX per-user） |

**为什么 release 也要跑测试**：16bpp "全黑"那个 bug 是整数溢出——debug 下 panic，
release 下静默回绕，debug 测试永远发现不了。这条已固化进验收脚本和 CI。

## 关键设计取舍

- **渲染优先 D3D12**（wgpu）。纯 OpenGL 在无物理显示器、或通过 RustDesk 等远程工具查看的
  机器上根本不呈现（应用内 framebuffer 回读证明画对了，swap 不出图 → 白屏）。回退链：
  `D3D12 → Vulkan → OpenGL(wgpu) → glow`，用 `RUSTVNC_RENDERER=dx12|vulkan|gl|glow` 可强制。
- **同一时刻只允许一个 `FramebufferUpdateRequest`**。多个请求在途会让服务端交错返回更新，
  5K 下表现为撕裂/果冻。
- **只在整帧收完后呈现**。服务端按矩形分块发送，收到一块就上屏会看到"自上而下扫描"。
- **自适应请求节奏**：帧延迟 EMA > 200 ms 连续出现就放宽间隔（上限 500 ms），回落到低位后
  恢复；画面静止 3 帧另降到 4 fps。可在 UI 关闭（关闭即锁定画质档帧率）。
- **双链路**：增强功能全部走 Agent，任何 Agent 故障都不该影响画面会话。

## 项目结构

```text
src/                        # macOS Agent：菜单栏、QUIC 控制面、剪贴板、音频、显示模式切换
crates/rustvnc-client/      # Windows 客户端：RFB 会话、egui UI、SSH、视图/多标签/托盘、更新器
crates/agent-client/        # Windows 侧 Agent 客户端：配对、剪贴板、Opus→WASAPI
crates/rfb-probe/           # 协议探针（真机编码/分辨率诊断）
vendor/vnc-rs/              # vnc-rs 0.6.1 + Apple/zlib/分块补丁（改动逐条记录在 PATCHES.md）
resources/                  # Info.plist、LaunchAgent、entitlements
scripts/                    # 构建 / 安装 / 公证 / 验收 / Apple 编码抓取
packaging/windows/          # Inno Setup、WiX MSI、签名与更新 manifest 示例
```

代码规模约 12.7k 行 Rust（不含 vendored 引擎与测试夹具）。

## 文档

- [总体设计（需求 / 架构 / 开发路径）](design.md)
- [macOS Agent 设计](agent-design.md)
- [M0 兼容性报告](M0-COMPATIBILITY.md) · [M1 客户端状态](M1-STATUS.md) · [**M2 执行计划**](M2-PLAN.md)
- [卡点分析与修复计划](BLOCKER-ANALYSIS.md) · [真机验收清单](ACCEPTANCE.md)
- Agent 侧分册：[A0 骨架](README-A0.md) · [A1 控制面](README-A1.md) · [A2 菜单栏与剪贴板](README-A2.md) · [A2b 音频](README-A2B.md)

## 自动化环境变量

无头或脚本化运行（soak、基准、验收）不必手工输入凭据：

| 变量 | 用途 |
|---|---|
| `MACOS_USERNAME` | macOS 账号；缺省时取已保存配置里的账号 |
| `MACOS_PASSWORD` | macOS 账号密码；设置了就用于本次会话，且**不写入凭据库**（测试/自动化专用） |
| `RUSTVNC_RENDERER` | 强制渲染后端：`dx12` / `vulkan` / `gl` / `glow` |
| `RUSTVNC_ALLOW_MULTIPLE` | 跳过单实例互斥（soak / 基准脚本需要独立被测进程时用） |

## 已知限制

- **多显示器**：仍是 macOS 合成的单个 framebuffer。客户端侧可按区域取景（整屏/左右上下半/
  自定义，只影响显示与鼠标映射，**带宽不变**）；真正的按显示器流需要 Agent 侧逐屏采集。
- **Apple 1105（显示几何）**：在 BetterDisplay 虚拟屏 + PointerPos + 指针移动 + 模式切换的
  真机实验中始终不发送，判定属于 Apple 高性能扩展；候选解析器仅作诊断保留，标准路径不依赖。
- **远端光标形状仅 32bpp**：16bpp（极速）不请求光标编码，仍显示本地指针。
- **D3D11 不可用**：wgpu 只有 DX12/Vulkan/GL 后端。仅 D3D11 级显卡的机器实际落到 GL 或 glow；
  要真 D3D11 需自带 ANGLE 或另写渲染器。
- **通知**：断线/重连/新版本会发系统通知，窗口在前台时不打扰；通知权限需在系统设置里授予。
- **签名与更新**：脚本与校验链路完备，但真实证书的 Authenticode、公证、以及公网更新服务器
  尚未配置；`RUSTVNC_UPDATE_URL` 可指向自建 manifest。
- **未实测**：8 小时 soak 与 4K60 本地呈现基准（1440p 已实测 59.9 fps，见 [M2-PLAN](M2-PLAN.md)）；真机输入 P95 仍需人工操作录制。
- **Windows 上 `cargo check --workspace` 会失败**：根包 `rustvnc-agent` 是 macOS-only，
  请按平台指定 `-p`。

## 故障排查

| 现象 | 处理 |
|---|---|
| 窗口空白/白屏 | 已在默认路径修复（wgpu/D3D12）。仍出现则 `RUSTVNC_RENDERER=glow` 对照，并附日志 |
| 画面撕裂、"果冻"、像在扫描 | 确认用 release 构建；再看状态栏 fps 与「静止降频」标记 |
| 极速（16bpp）偏色/发黑 | 0.1.0 已修（RGB565 通道缩放溢出）；老版本请升级 |
| SSH 认证失败 | 检查 OpenSSH 版本（密码需 8.4+/Windows 8.6+），或改用密钥 |
| 收不到系统通知 | 先跑 `.\target\release\rustvnc-client.exe --test-notification` 自测；仍不弹就查“设置 → 系统 → 通知”里 RustVNC 是否被关掉、勿扰模式是否开启 |
| 日志位置 | `%APPDATA%\RustVNC\client\logs`；"诊断" 页可一键导出诊断包 |

## 许可

MIT，见 [LICENSE](LICENSE)。
