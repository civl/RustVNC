# RustVNC

RustVNC 是一个用 Rust 构建、面向 Windows 用户的 macOS 远程桌面项目。

它由两条并行的链路组成：

```text
Windows 客户端 ── RFB/SSH (5900) ──────────► macOS screensharingd   画面 + 输入
               └─ QUIC/TLS 1.3 (8443) ─────► RustVNC Agent         剪贴板 + 音频 + 控制
```

- **原生引擎**（零安装）：标准 RFB，ZRLE / zlib / CopyRect / Raw，Apple ARD 账号认证与 VNC 密码认证。
- **增强引擎**（在 Mac 上装 Agent）：剪贴板双向、Opus 系统音频、mTLS 配对；画面仍走 RFB，Agent 出问题只丢功能、不丢会话。

## 文档

- [总体设计（需求 / 架构 / 开发路径）](design.md)
- [macOS Agent 设计（B1/B2、音频、视频、权限、配对）](agent-design.md)
- [A0 骨架说明](README-A0.md) · [A1 控制面](README-A1.md) · [A2 菜单栏与剪贴板](README-A2.md) · [A2b 音频](README-A2B.md)
- [M0 兼容性报告](M0-COMPATIBILITY.md)
- [M1 / M1b 客户端状态报告](M1-STATUS.md)
- [卡点分析与修复计划](BLOCKER-ANALYSIS.md)
- [验收清单（真机）](ACCEPTANCE.md)

## 当前进度

> **M0** 是协议里程碑；**A0–A2b** 是 macOS Agent 侧；**M1/M1b** 是 Windows 客户端侧。

### 已实现并经测试验证

| 能力 | 状态 | 验证方式 |
|---|---|---|
| RFB 握手 / Apple ARD / VNC 密码 | ✅ | mock 端到端 + 真机握手（M0 报告） |
| ZRLE / **zlib(6)** / CopyRect / Raw | ✅ | `encoding_compat` 端到端解码比对像素 |
| Apple 私有编码 1104/1105/1107/1109/1110 | ⚠️ 安全跳过 | 未协商也不致命；**内容未解析**，需真机抓包确认 |
| 未知编码 | ✅ 明确报错 | 一次性失败并给出编码号，不进入重连风暴 |
| 键鼠输入 / 修饰键状态机 / 特殊键菜单 / 按键映射 | ✅ | 单测 + 物理键位映射 |
| 连接管理 / 配置 CRUD / 导入导出 | ✅ | 单测 |
| 凭据存储（Credential Manager / Keychain） | ✅ | 单测（内存后端） |
| 自动重连（指数退避，短会话不重置） | ✅ | mock 测试 |
| 空闲 CPU | ✅ 0.000s / 3s | `idle_cost` 回归测试（曾为 100% 单核） |
| 连接 / 握手超时 | ✅ 10s / 15s | 代码路径 + 超时分支 |
| **Agent 通道：配对 / 剪贴板 / 音频 / 分辨率** | ✅ | QUIC+TLS 回环；真机 5K→1080p→5K |
| SSH 隧道：host key 首次信任、stderr 透出、密钥生成 | ✅ | 单测（known_hosts 匹配、命令加固、错误提示） |
| 诊断日志 / 诊断包 | ✅ | 手动冒烟 |

### 已知限制

- **SSH 密码认证需要新版 OpenSSH**：现在通过 `SSH_ASKPASS`（客户端自身以
  `--ssh-askpass` 模式运行）支持密码，Unix 需 OpenSSH 8.4+、Windows 需 8.6+；
  旧版仍会明确提示改用密钥（提供一键生成 ed25519 密钥 + `authorized_keys` 指引）。
- **Apple 私有编码的内容未解析**：默认不请求（配置里可开“Apple 扩展”），
  只有真机抓包确认 1105/1109 的字节布局后才有意义。
- **真机指标未测**：1080p 30 分钟稳定性、帧率、输入 P95 需要一台可连的 Mac +
  Windows 真机，见 [ACCEPTANCE.md](ACCEPTANCE.md)。
- **自适应帧率**：画面静止 3 帧后从设定帧率降到 4 fps（减少服务端重算与带宽），任何一帧带像素就立刻恢复；状态栏显示「静止降频」以免把低 fps 误判为卡顿。
- **远端分辨率（实验性）**：仅主显示器、仅会话开始时切换；客户端可选 1080p/1440p/4K，
  需要 Mac 菜单栏“允许远程调整分辨率”开启；断开或崩溃后恢复原模式，运行中不动态改。
- **远端光标形状**：32bpp 画质下跟随服务器下发的光标位图（热点、掩码都已处理），鼠标进入远程画面时隐藏本地指针、改画远端形状；16bpp（极速）不请求光标编码，仍显示本地指针。
- **渲染后端按 D3D12 → Vulkan → OpenGL(wgpu) → glow 的顺序尝试**，第一个能初始化的生效。
  Windows 上优先 **D3D12**：它是 DWM 合成与远程抓屏（RustDesk/RDP）真正预期的 API，
  而纯 OpenGL 在无物理显示器/间接显示器上**根本不呈现**（应用内 framebuffer 回读证明画对了
  但 swap 不出图，表现为白板）。
  - `RUSTVNC_RENDERER=dx12|vulkan|gl|glow` 可强制单一后端（排查用）
  - **D3D11 无法通过 wgpu 获得**（wgpu 只有 DX12/Vulkan/GL 后端）。若目标机器仅有 D3D11
    级显卡，实际落点是 wgpu 的 GL 后端或 glow；真要 D3D11 需自带 ANGLE 或写自定义渲染器，
    当前不做。`RUSTVNC_RENDERER=gl` 可显式走 wgpu 的 OpenGL 后端做对照。
- Agent 安装包需要 Developer ID 签名 + 公证才能分发给他人（脚本已就绪，证书自备）。

## 目录

```text
src/                        # macOS Agent（菜单栏、QUIC 控制面、剪贴板、音频）
crates/rustvnc-client/      # Windows 客户端（RFB 会话 + egui UI + SSH）
crates/agent-client/        # Windows 侧 Agent 客户端（配对、剪贴板、Opus→WASAPI）
crates/rfb-probe/           # M0 协议探针
vendor/vnc-rs/              # vnc-rs 0.6.1 + Apple 编码/zlib 补丁（见 PATCHES.md）
resources/                  # Info.plist、LaunchAgent plist、entitlements
scripts/                    # 构建 / 安装 / 公证 / 验收
packaging/windows/          # Inno Setup + 签名脚本
```

## 构建与测试

```bash
# Windows 客户端 + Agent 客户端 + 探针 + 协议引擎
cargo test -p rustvnc-client -p rustvnc-agent-client -p rfb-probe -p vnc-rs
cargo clippy -p rustvnc-client -p rustvnc-agent-client -p rfb-probe -p vnc-rs --all-targets -- -D warnings

# macOS Agent（只能在 macOS 上编译）
cargo test --workspace
./scripts/build-app.sh release
```

> 根包 `rustvnc-agent` 是 macOS-only，所以 Windows 上 `cargo check --workspace`
> 会失败；CI 按平台拆成 `macos` 与 `windows-client` 两个 job。
>
> 客户端请用 **release** 运行：5K 画面下 debug 构建的解码慢到肉眼可见（拖窗口像在扫屏），
> `cargo build --release -p rustvnc-client` 后再跑。

一键验收：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/acceptance.ps1
```

## 许可

MIT，见 [LICENSE](LICENSE)。
