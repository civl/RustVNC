# RustVNC Agent（macOS 伴生代理）设计

> 版本 v0.1（草案） · 2026-11-16 · 范围：**LAN / Tailscale，不做公网中继**
>
> 关联文档：[design.md](design.md)（总体方案，§6.5 RustDesk 参考边界，M4/M5 里程碑）
>
> 实现状态（2026-11-16）：A0 骨架、A1 控制面（QUIC + TLS 1.3 + CSR 配对 + Keychain + 按 IP 限流 + Ping/Pong + 集成测试）已落地；
> 实现细节见 [README-A1.md](README-A1.md)。

---

## 0. 摘要

**一句话定位**：在 Mac 上运行一个菜单栏常驻的轻量代理，为 Windows 客户端补齐原生 VNC 缺失的**音频、剪贴板、强加密**，并可选提供 **SCK + VideoToolbox 的高画质视频通道**；所有连接只发生在局域网或 Tailscale 内，不依赖任何公网服务。

**四个关键决策**：

1. **B1 为默认，B2 为增强**。B1 = 原生 VNC 画面 + Agent 音频/剪贴板（稳定、权限最小；画面支持登录窗口，音频/剪贴板在用户登录后可用）；B2 = Agent 自己采集/编码视频（高画质、低延迟，但需要屏幕录制 + 辅助功能权限，且只能在用户登录后工作）。
2. **不复刻 RustDesk，也不从零造轮子**。采集/编码走 Apple 官方 SCK + VideoToolbox；传输走 `quinn` + `rustls`；音频走 Core Audio Tap + Opus；只借鉴 RustDesk 的模块划分与权限引导思路，不复制 AGPL 代码。
3. **Tailscale 是传输底座，不是安全替代品**。Agent 只绑定 Tailscale/LAN 地址，TLS 1.3 + mTLS + 证书固定照做，纵深防御。
4. **稳定性优先于峰值性能**。launchd 监督、看门狗、组件级重启、有界队列、丢帧不堆积；性能优化（零拷贝、NV12、低延迟码控）在不破坏稳定性的前提下逐步开启。

| 关键指标 | 目标 |
|---|---|
| B1 CPU（M1，音频+剪贴板） | < 5% |
| B1 音频端到端延迟 | 60–100 ms（LAN/Tailscale 直连） |
| B2 1080p30 CPU | 10–20%（M1） |
| B2 1080p60 CPU | 15–25%（M1） |
| B2 端到端延迟 | 40–80 ms（LAN） |
| 常驻内存 | B1 < 80 MB；B2 150–300 MB |
| 冷启动到可连接 | < 3 s |
| 8 小时 soak | 0 崩溃、内存增长 < 5% |

---

## 1. 范围与目标

### 1.1 目标

- **G1 补齐能力**：在原生 VNC 之上提供系统音频、UTF-8 剪贴板和 TLS 1.3 加密，不改变客户端的 VNC 主体流程。
- **G2 可选高画质**：通过 SCK + VideoToolbox 提供 1080p/1440p/4K 的 H.264（可选 HEVC）视频，码率可调。
- **G3 零运维**：菜单栏安装、登录自启、权限引导、自动更新、崩溃自恢复；用户不需要碰命令行。
- **G4 最小权限**：B1 只申请“系统音频录制”；只有开启 B2 才申请屏幕录制与辅助功能。
- **G5 边界清晰**：只监听 Tailscale/LAN 地址，不做 NAT 穿透、不做公网中继、不做 UPnP。

### 1.2 非目标

- 公网直连、NAT 穿透、rendezvous/relay（交给 Tailscale，或未来单独立项）。
- 文件传输、远程打印、会话录制、多人协作。
- 麦克风上行（客户端说话→Mac）——v2 再评估。
- 登录窗口之前的音频/剪贴板（用户 LaunchAgent 尚未启动；此时用原生 VNC 路径）。
- 复刻 Apple“高性能屏幕共享”私有协议。

### 1.3 与总体设计的关系

`design.md` 定义了 Windows 客户端、原生 RFB 引擎、双引擎架构与 25/29 周路线。本文件只负责 **macOS 侧 Agent**；两者冲突时，Agent 细节以本文件为准（例如 B1 默认不代理 RFB、音频走数据报）。

| 主题 | design.md | 本文件 |
|---|---|---|
| 客户端 UI/渲染 | 定义 | 不涉及 |
| 原生 RFB 协议 | 定义 | B1 不代理、不修改 |
| Agent 传输/配对 | 概要 | 详细设计 |
| 音频/剪贴板 | 需求 | 详细设计 |
| B2 视频 | M5 可选 | 详细设计 |
| 安装/权限/更新 | 概要 | 详细设计 |

### 1.4 融合原则（借鉴什么、不借鉴什么）

| 来源 | 采用 | 不采用 |
|---|---|---|
| Apple 官方文档（SCK / VideoToolbox / Core Audio / ServiceManagement） | 全部走公开 API 与推荐配置 | 私有 SPI、私有 TCC 绕过 |
| RustDesk（AGPL-3.0） | 模块划分、权限引导、音频链路分层、弱网恢复的**思路** | 任何代码、自有协议、Flutter UI、relay |
| `scap`（MIT，RustDesk fork） | 仅作采集回退与对照 | 主路径（其 macOS 后端是已弃用的 CGDisplayStream） |
| `enigo`（MIT） | 可选输入实现/对照 | 全盘依赖（B2 直接 CGEvent 更可控） |
| `quinn` / `rustls` / `opus` / `cpal` | 直接依赖 | — |
| 自研 | B1/B2 双模、零拷贝管线、Tailscale 绑定、launchd 监督、配对模型 | — |

> 原则：**能复用成熟库就复用，能用官方 API 就不用私有实现，必须自研的部分才自研**；所有第三方代码进 `deny.toml` 白名单。

---

## 2. 部署与网络模型

### 2.1 拓扑

```text
┌──────────────────────┐        Tailscale（WireGuard）        ┌──────────────────────┐
│ Windows 客户端        │◄────────────────────────────────────►│ macOS 被控端          │
│                      │                                      │                      │
│  RFB 引擎 ───────────┼── 5900/TCP（原生 VNC，SSH 或直连） ──►│ screensharingd        │
│                      │                                      │                      │
│  Agent 客户端 ───────┼── 8443/UDP（QUIC）或 TCP（TLS） ────►│ RustVNC Agent        │
│   ├ 音频             │                                      │  ├ Core Audio Tap    │
│   ├ 剪贴板           │                                      │  ├ NSPasteboard      │
│   ├ 控制/统计        │                                      │  ├ （可选）SCK 视频   │
│   └ （可选）视频     │                                      │  └ （可选）CGEvent    │
└──────────────────────┘                                      └──────────────────────┘
```

- **B1**：5900 与 8443 是两条独立连接；Agent 不代理 RFB，不碰 VNC 认证。
- **B2**：客户端只连 8443；Agent 承担视频/音频/输入。
- 两条路径都只在 Tailscale 或局域网内，不经过任何云服务。

### 2.2 绑定策略

Agent 启动时枚举本机地址，按优先级绑定：

| 优先级 | 地址 | 说明 |
|---|---|---|
| 1 | Tailscale IPv4（`100.64.0.0/10`） | 默认首选；MagicDNS 名称可直接用 |
| 2 | 局域网 IPv4（RFC1918） | Tailscale 未运行时的回退 |
| 3 | `127.0.0.1` | 仅本机调试 |
| 4 | `0.0.0.0` | 仅在用户显式选择“所有接口”时启用，并红色警告 |

配置项：

```toml
[network]
bind = "auto"            # auto | tailscale | lan | all | 具体地址
port = 8443
enable_tcp_fallback = true
```

- `auto`：优先 Tailscale 地址；若没有，则绑定第一个 RFC1918 地址；两者都没有则报错并提示安装/启动 Tailscale。
- 地址变化（DHCP、Tailscale 重连）通过 `getifaddrs` 每 5 秒检测；变化后重建监听 socket，并在菜单栏提示。
- 如果同时需要 Tailscale 与 LAN 访问：绑定两个 socket（`socket2` 分别 bind），不要用 `0.0.0.0` 图省事。

### 2.3 端口与 MTU

| 项 | 值 | 说明 |
|---|---|---|
| QUIC | UDP 8443 | 首选；多路复用 + 数据报 |
| TLS/TCP | TCP 8443 | 可选回退；QUIC 不可用时 |
| MTU | 1280（Tailscale） | WireGuard 隧道固定 1280 |
| QUIC 最大 UDP 载荷 | 1200 | 留出头部余量，避免 IP 分片 |
| QUIC initial MTU | 1200 | 不做激进 PMTUD |
| 保活 | 5 s | 保持 NAT/隧道映射 |
| 空闲超时 | 30 s | 客户端断开后释放资源 |

**不要**依赖 1500 MTU：Tailscale 的 `utun` 是 1280，超过会被静默丢弃。[Tailscale MTU 文档](https://tailscale.com/docs/reference/troubleshooting/network-configuration/tcp-connection-two-devices)

### 2.4 Tailscale ACL 建议

只放行必要端口，示例（按实际 tag 调整）：

```json
{
  "grants": [
    {
      "src": ["tag:windows"],
      "dst": ["tag:mac"],
      "ip": ["udp:8443", "tcp:8443", "tcp:5900"]
    }
  ]
}
```

- 不把 8443 暴露给整个 tailnet；按设备/用户限制。
- 如果只用 B1 且 RFB 走 SSH，`tcp:5900` 可只对 SSH 跳板机开放。
- 禁用 Tailscale 的 Funnel（不要公网暴露）。

### 2.5 无中继、无公网暴露

- 不实现 rendezvous/relay；Tailscale 的 DERP 只作为其自身的回退，不改变我们的“局域网/Tailnet”边界。
- 不调用 UPnP/NAT-PMP；不监听公网地址。
- 若 Tailscale 与 LAN 都不可用，Agent 进入“仅本机”状态并在菜单栏报错，不静默降级到 `0.0.0.0`。

---

## 3. 形态与进程模型

### 3.1 应用形态

- 一个签名的 **`.app` bundle**：`RustVNC Agent.app`，安装到 `/Applications`。
- `LSUIElement = true`：不显示 Dock 图标，只在菜单栏常驻。
- 主可执行文件 = Rust 二进制；内部包含网络、采集、编码、音频、输入、剪贴板。
- 不拆独立 daemon：减少 TCC 归属、IPC、生命周期复杂度。

### 3.2 登录自启与监督

- 使用 **`SMAppService.agent(plistName:)`**（macOS 13+）注册 `Contents/Library/LaunchAgents/com.rustvnc.agent.plist`。
- plist 关键项：

```xml
<key>Label</key><string>com.rustvnc.agent</string>
<key>ProgramArguments</key>
<array><string>/Applications/RustVNC Agent.app/Contents/MacOS/rustvnc-agent</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key>
<dict><key>SuccessfulExit</key><false/></dict>
<key>ProcessType</key><string>Interactive</string>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>ThrottleInterval</key><integer>10</integer>
<key>ExitTimeOut</key><integer>5</integer>
```

- 为什么用 `SMAppService.agent` 而不是往 `~/Library/LaunchAgents` 扔 plist：plist 在签名 bundle 内，第三方无法篡改；用户可在“登录项”里看到并关闭。
- 为什么是 `Aqua` 会话：SCK、CGEvent、NSPasteboard、菜单栏都需要用户会话；LaunchDaemon（root）拿不到这些能力。

### 3.3 菜单栏

- 技术栈：`tao` 事件循环 + `tray-icon`（Tauri 同栈）+ `objc2`（NSAlert/NSWorkspace/ServiceManagement/TCC）。
- 菜单项：
  - 状态：运行中 / 暂停 / 错误（带简短原因）
  - 权限：屏幕录制、辅助功能、系统音频（各自状态 + 一键打开系统设置）
  - 配对：显示 8 位配对码 + 指纹短语（5 分钟有效）
  - 模式：自动 / 仅 B1 / 启用 B2
  - 打开日志、复制诊断信息、退出
- 不在菜单里做复杂设置；详细配置写入 `~/Library/Application Support/RustVNC/agent.toml`，由客户端远程修改或手工编辑。
- 图标状态：灰色（未连接）、绿色（已连接）、橙色（降级/权限缺失）、红色（错误）。

### 3.4 单实例与崩溃恢复

- 启动时对 `~/Library/Application Support/RustVNC/agent.lock` 加 `flock`；拿不到锁说明已有实例，弹窗后退出。
- `KeepAlive = { SuccessfulExit: false }` + `ThrottleInterval=10`：崩溃（非 0 退出）后 launchd 自动拉起，但 10 秒内不重复拉起；用户主动“退出”（0 退出）不会被拉回，直到下次登录或手动打开。
- 状态持久化到 `state.json`（原子写）：上次模式、端口、配对客户端指纹、最后错误；启动时恢复。
- 崩溃转储：默认写 `~/Library/Logs/RustVNC/`，不含屏幕内容/按键/剪贴板；用户可一键清理。

### 3.5 为什么不是 SwiftUI / 独立 daemon

| 方案 | 问题 |
|---|---|
| SwiftUI 壳 + Rust 静态库 | 多一套工具链与 FFI 边界；菜单栏功能很薄，收益低 |
| Rust 无 UI daemon + 独立菜单栏 App | 两个进程、两套 TCC 归属、IPC 复杂；崩溃隔离收益不抵成本 |
| 纯 Rust + `tao`/`tray-icon` | **采用**：单一语言、单一 bundle、TCC 归属清晰、Tauri 已验证 |

> 如果后续设置界面变复杂（多显示器选择、码率曲线、日志查看器），再评估加一个 SwiftUI 设置窗口；v1 保持菜单栏 + 配置文件。

---

## 4. 两种工作模式

### 4.1 B1：原生 VNC + Sidecar（默认）

```text
客户端 ── RFB（5900，SSH/Tailscale）──► screensharingd
客户端 ── QUIC/TLS（8443）───────────► Agent ── Core Audio Tap / NSPasteboard
```

- **视频/输入**：完全走原生 VNC，Agent 不参与、不代理、不改协议。
- **连接模型**：默认 B1a——RFB 与 Agent 是两条独立连接；如果产品要求“单连接”，可选 B1b 由 Agent 在 QUIC 流里代理 5900（本文件按 B1a 设计，B1b 仅预留）。
- **音频**：Agent 采集系统音频 → Opus → QUIC 数据报 → 客户端播放。
- **剪贴板**：Agent 监听 `NSPasteboard` ↔ 客户端文本。
- **权限**：只需要“系统音频录制”（Core Audio Tap，macOS 14.2+）；不需要屏幕录制、不需要辅助功能。
- **登录窗口**：RFB 路径天然支持；此时 Agent 还没启动（用户 LaunchAgent 未运行），登录后自动可用。
- **稳定性**：最高；视频路径与原生 VNC 完全一致，Agent 崩溃不影响画面。

> B1 是方案 C 的“稳定底座”。它不追求画质，只补齐音频/剪贴板/加密。

### 4.2 B2：Agent 原生视频（可选，M5）

```text
客户端 ── QUIC/TLS（8443）──► Agent ── SCK 采集 → VideoToolbox 编码 → 视频流
                                      ├ Core Audio Tap → Opus
                                      ├ CGEvent 输入注入
                                      └ NSPasteboard
```

- **视频**：SCK 采集 → VideoToolbox 硬编 H.264/HEVC → QUIC 流。
- **输入**：CGEvent 注入；需要辅助功能权限。
- **权限**：屏幕录制 + 辅助功能 + 系统音频。
- **限制**：只能在用户登录后工作；登录窗口、FileVault 预启动、部分密码框（Secure Input）无法注入。
- **画质**：1080p/1440p/4K，30/60 fps，码率 2–50 Mbps 可调。
- **稳定性**：复杂度最高；必须有 B1 回退。

### 4.3 能力矩阵

| 能力 | B1 | B2 | 备注 |
|---|---|---|---|
| 画面 | 原生 VNC | SCK + VT | B1 受 VNC 编码限制 |
| 输入 | RFB | CGEvent | B2 需要辅助功能 |
| 音频 | ✅ | ✅ | Core Audio Tap 优先 |
| 剪贴板 | ✅ | ✅ | 文本 |
| 登录窗口 | ✅（RFB） | ❌ | B2 需用户会话 |
| 屏幕录制权限 | ❌ | ✅ | B1 不需要 |
| 辅助功能权限 | ❌ | ✅ | B1 不需要 |
| 音频录制权限 | ✅ | ✅ | 14.2+ |
| 画质可调 | 有限（VNC） | 码率/帧率/分辨率 | — |
| 崩溃影响 | 仅音频/剪贴板 | 全部 | B1 更稳 |

### 4.4 模式选择

- 客户端配置：`mode = auto | b1 | b2`；默认 `auto`。
- `auto` 逻辑：
  1. 先建立 RFB（登录窗口/桌面都能用）。
  2. 尝试连接 Agent 8443；成功则启用音频/剪贴板（B1）。
  3. 如果 Agent 通告了 B2 能力、权限齐全、用户开启“高画质”，则升级到 B2；失败自动回退 B1。
- Agent 在 `ServerHello` 中返回 `capabilities`（audio/clipboard/video/screen_recording/accessibility），客户端据此选择。
- 模式切换必须无黑屏：B2 启动后先发首帧，客户端收到再关闭 RFB；失败则保持 RFB。

---

## 5. 权限与隐私

### 5.1 TCC 矩阵

| 能力 | API | TCC 服务 | Info.plist | B1 | B2 |
|---|---|---|---|---|---|
| 系统音频 | Core Audio Tap | `kTCCServiceAudioCapture` | `NSAudioCaptureUsageDescription` | ✅ | ✅ |
| 系统音频（回退） | SCK audio | Screen Recording | — | 可选 | 可选 |
| 屏幕画面 | SCK | Screen Recording | — | ❌ | ✅ |
| 键鼠注入 | CGEvent | Accessibility | — | ❌ | ✅ |
| 剪贴板 | NSPasteboard | 无（macOS 15+ 可能提示） | — | ✅ | ✅ |

说明：

- 屏幕录制没有专用 usage description 键；首次调用 SCK 时由系统弹窗。
- Core Audio Tap 需要 `NSAudioCaptureUsageDescription`；macOS 14.2+ 引入，14.4+ 更稳定，需真机矩阵验证。
- 辅助功能通过 `AXIsProcessTrustedWithOptions` 检查并引导；未授权时 B2 输入禁用，但视频/音频仍可用。
- 不申请 Full Disk Access、不申请 root、不安装 LaunchDaemon。

### 5.2 权限引导

首次启动向导（3 步，可跳过）：

1. **系统音频**：说明用途（“把 Mac 的声音传到 Windows”），点击“授权”触发系统弹窗；失败时按系统版本打开“隐私与安全性 → 屏幕与系统音频录制”（或“音频录制”）。
2. **屏幕录制**（仅 B2）：说明“只有开启高画质才需要”；用 `CGPreflightScreenCaptureAccess` 检查，必要时 `CGRequestScreenCaptureAccess` 触发系统弹窗。
3. **辅助功能**（仅 B2）：说明“远程键鼠需要”，打开“隐私与安全性 → 辅助功能”。

菜单栏始终显示权限状态；权限被撤销时：
- 视频/音频停止，状态变橙色；
- 菜单给出“重新授权”；
- 日志记录 `permission_revoked`，不上报内容。

### 5.3 隐私

- 屏幕/音频只在有客户端连接时采集；未连接时停止 SCK/Tap，CPU 接近 0。
- 菜单栏图标 + 系统自带的屏幕录制/麦克风指示器双重可见。
- 剪贴板默认开启但限长 1 MB；提供“仅手动同步”开关；锁屏时暂停。
- 音频不落盘；视频不落盘；日志不含画面、按键、剪贴板内容。
- 配对/连接日志只记录设备名、证书指纹、时间、字节数，保留 7 天。

### 5.4 系统版本差异

| 版本 | 差异 | 处理 |
|---|---|---|
| macOS 14.0–14.1 | Core Audio Tap 不可用 | 回退 SCK audio（需屏幕录制）或禁用音频 |
| macOS 14.2+ | Tap 可用 | 默认 Tap |
| macOS 15+ | 屏幕与系统音频权限更细、剪贴板可能提示 | 引导页按版本分支；失败回退手动 |
| macOS 26/27 | API 可能调整 | 每次系统升级跑兼容矩阵 |

> 所有版本差异以《Agent 兼容性报告》实测为准；不在代码里硬编码“某版本一定可用”。

---

## 6. 模块与线程模型

### 6.1 模块划分

```text
apps/macagent/
├─ src/
│  ├─ main.rs              # 入口、单实例、launchd 参数
│  ├─ app.rs               # tao/AppKit 事件循环、菜单栏、激活策略
│  ├─ menu.rs              # 菜单项、状态图标、配对码弹窗
│  ├─ permissions.rs       # TCC 检查/引导、系统设置跳转
│  ├─ config.rs            # agent.toml、默认值、原子写
│  ├─ state.rs             # 运行状态、能力、会话
│  ├─ net/
│  │  ├─ quic.rs           # quinn endpoint、连接、流
│  │  ├─ tls.rs            # rustls 配置、证书校验、pinning
│  │  ├─ framing.rs        # 长度前缀、消息编解码
│  │  └─ tcp_fallback.rs   # 可选 TCP/TLS
│  ├─ auth/
│  │  ├─ ca.rs             # 自签 CA、证书签发
│  │  ├─ pairing.rs        # 配对码、限流、状态机
│  │  └─ keychain.rs       # SecItem 存取
│  ├─ capture/
│  │  ├─ sck.rs            # SCShareableContent/SCStream
│  │  ├─ display.rs        # 显示器枚举、坐标、缩放
│  │  └─ frame.rs          # CVPixelBuffer 封装、池
│  ├─ encode/
│  │  ├─ vt.rs             # VTCompressionSession
│  │  ├─ annexb.rs         # AVCC → Annex-B、SPS/PPS
│  │  └─ bitrate.rs        # 码率/关键帧策略
│  ├─ audio/
│  │  ├─ tap.rs            # Core Audio Tap + aggregate device
│  │  ├─ sck_audio.rs      # SCK audio 回退
│  │  ├─ resample.rs       # rubato
│  │  └─ opus.rs           # Opus 编码
│  ├─ input/
│  │  ├─ keymap.rs         # Windows VK → macOS keycode
│  │  ├─ cgevent.rs        # 键鼠/滚轮/Unicode 注入
│  │  └─ secure_input.rs   # Secure Input 检测
│  ├─ clipboard.rs         # NSPasteboard 轮询/去重
│  ├─ session.rs           # B1/B2 会话、能力协商
│  ├─ watchdog.rs          # 健康检查、组件重启
│  └─ metrics.rs           # 计数器、日志、诊断包
└─ resources/
   ├─ Info.plist
   ├─ com.rustvnc.agent.plist
   └─ AppIcon.icns
```

### 6.2 线程/队列

| 线程/队列 | 职责 | 规则 |
|---|---|---|
| Main（tao/AppKit） | 菜单栏、弹窗、权限 UI | 不跑网络/编码；重活 dispatch 到后台 |
| Tokio core（多线程） | QUIC/TLS、控制消息、剪贴板轮询、看门狗 | 所有 await 有超时 |
| SCK callback queue（串行） | 接收 `CMSampleBuffer` | 只做轻量封装 + 入队；`autoreleasepool` |
| VT encode queue（串行） | `VTCompressionSessionEncodeFrame` | 最多 2 帧在飞；满则丢旧帧 |
| Audio IOProc（实时） | 收 PCM | 不分配、不锁、不日志；写 `rtrb` |
| Audio encode（专用） | 重采样 + Opus | 有界队列；满则丢最旧 |
| Clipboard timer（Tokio） | 500 ms 轮询 | 失败退避 |
| Watchdog（Tokio） | 2 s 健康检查 | 只做重启决策，不做重活 |

### 6.3 数据流

**B1**

```text
NSPasteboard ──► clipboard ──► control stream ──► 客户端
Core Audio Tap ──► ring ──► Opus ──► datagram ──► 客户端
客户端 ──► control stream ──► clipboard ──► NSPasteboard
```

**B2**

```text
SCK ──► CVPixelBuffer(IOSurface) ──► VT encode ──► Annex-B ──► video stream ──► 客户端
Tap/SCK audio ──► ring ──► Opus ──► datagram ──► 客户端
客户端 ──► input stream ──► CGEvent
```

### 6.4 背压与丢弃

- **视频**：编码队列 ≤ 2 帧；发送队列 ≤ 2 帧；超过则丢最旧的非关键帧；丢帧后请求关键帧（或等下一个周期关键帧）。
- **音频**：Opus 队列 ≤ 200 ms；超过丢最旧；绝不阻塞 IOProc。
- **控制/剪贴板**：有界通道（32 条）；满时断开异常客户端，避免内存膨胀。
- **统计**：原子计数器，不参与热路径锁。

---

## 7. 传输层

### 7.1 QUIC + TLS 1.3

- `quinn` + `rustls`，TLS 1.3 only，ALPN `rustvnc-agent/1`。
- 禁用 0-RTT（配对/控制消息不可重放）。
- 服务端要求客户端证书（配对完成后）；配对模式允许无客户端证书，但必须带配对码。
- 证书校验：客户端固定 Agent SPKI（SHA-256），Agent 校验证书链 + 客户端指纹白名单。

### 7.2 流与数据报

| 通道 | 类型 | 可靠性 | 用途 |
|---|---|---|---|
| control | 双向流 | 可靠有序 | Hello、能力、配置、统计、暂停 |
| input | 单向流 | 可靠有序 | 键鼠事件（B2） |
| clipboard | 双向流 | 可靠有序 | UTF-8 文本 |
| video | 单向流 | 可靠有序 | 编码帧（B2） |
| audio | QUIC datagram | 不可靠 | Opus 包（低延迟） |

- 音频不走可靠流：避免被视频/控制消息的队头阻塞拖慢。
- 视频不拆到多条流：编码顺序即显示顺序，单流足够；丢包由 QUIC 重传，延迟敏感时由客户端请求关键帧。
- 若 `quinn` 的 datagram 扩展不可用，回退为独立单向流 + 客户端抖动缓冲。

### 7.3 帧格式

所有流消息使用 `u32` 长度前缀（大端），最大 8 MB。

**VideoFrame**

```text
u32 length
u8  type = 0x01
u8  flags        # bit0 keyframe, bit1 config
u16 reserved
u64 pts_us       # 采集时刻（host clock）
u32 width
u32 height
u32 payload_len
u8  payload[payload_len]   # Annex-B（SPS/PPS/IDR/P）
```

**AudioPacket（datagram）**

```text
u16 seq
u32 pts_us
u8  flags        # bit0 FEC, bit1 DTX
u8  payload_len
u8  payload[payload_len]   # Opus
```

**Control（postcard）**

```rust
enum ControlMsg {
    Hello { proto: u16, client: String, caps: u32 },
    ServerHello { proto: u16, agent: String, caps: u32, display: DisplayInfo },
    StartSession { mode: Mode, audio: bool, clipboard: bool, video: Option<VideoConfig> },
    SetBitrate { bps: u32 },
    SetFps { fps: u16 },
    RequestKeyframe,
    Pause,
    Resume,
    ClipboardText { text: String },
    Stats(Stats),
    Ping(u64),
    Pong(u64),
    Error { code: u16, message: String },
}
```

### 7.4 流控与超时

| 参数 | 值 | 理由 |
|---|---|---|
| `max_udp_payload_size` | 1200 | Tailscale MTU 1280 |
| `initial_mtu` | 1200 | 避免启动期分片 |
| `keep_alive_interval` | 5 s | 保持隧道映射 |
| `max_idle_timeout` | 30 s | 客户端消失后释放 |
| 控制流写超时 | 5 s | 防止半死连接 |
| 视频流写超时 | 2 s | 卡住即丢帧/重连 |
| 握手超时 | 10 s | 防慢速攻击 |
| 配对码有效期 | 5 min | 降低暴力破解窗口 |

### 7.5 重连与会话恢复

- 客户端断开：Agent 立即停止视频/音频采集，保留 30 秒“热”状态（配对、能力）；超时后回到空闲。
- 重连：新 QUIC 连接 → mTLS → `Hello` → `StartSession`；Agent 重新创建 SCK/VT/音频，发首帧关键帧。
- 不做跨连接的状态迁移（视频参考帧、Opus 状态重置）；简单可靠优先。
- 网络切换：客户端侧负责重连；Agent 检测到地址变化时重建 listener，不影响已有连接（已有连接走旧路径直到断开）。

### 7.6 TCP/TLS 回退（可选）

- 同一端口 TCP 8443，TLS 1.3，同样的长度前缀协议。
- 音频改为可靠流 + 客户端抖动缓冲；视频同样走流。
- 仅当 QUIC 连续失败 3 次时启用；默认开启，可在配置关闭。
- 实现优先级低于 QUIC；M4 可先只做 QUIC，M4b 再补 TCP。

---

## 8. 认证与配对

### 8.1 证书体系

- Agent 首次运行生成 **自签 CA**（P-256 ECDSA，`rcgen`）；CA 私钥存 Keychain。
- Agent 服务端证书由该 CA 签发，SAN 包含：
  - `rustvnc-agent.local`
  - Tailscale MagicDNS 名称（如 `mac-mini.tailnet.ts.net`）
  - 当前 Tailscale/LAN IP
- 客户端配对后获得由同一 CA 签发的客户端证书（有效期 1 年）。
- 客户端固定 Agent 证书 SPKI；Agent 维护客户端证书指纹白名单（`clients.json`，0600）。

> 不用公共 CA、不做 ACME：Tailscale 内没有公网域名，自签 + pinning 更简单、更安全。

### 8.2 配对流程

```text
Agent（菜单栏）                         Windows 客户端
  │ 生成 8 位配对码 + 指纹短语              │
  │ 显示 5 分钟                            │
  │◄──────── TLS（未 pin，允许配对模式）────│
  │◄──────── PairRequest{code, csr, name} ──│
  │ 校验 code（常量时间）+ 限流              │
  │ 签发客户端证书                          │
  │───────── PairResponse{cert, ca} ───────►│
  │ 保存客户端指纹                          │ 保存证书/私钥 + pin Agent SPKI
  │                                        │
  │◄──────── 重新连接（mTLS）───────────────│
```

要点：

- 配对码 8 位数字，`OsRng` 生成，5 分钟过期，一次性。
- 配对码防的是“同网段陌生人”，**不防主动 MITM**；首次配对必须核对指纹短语，并建议在可信的 Tailscale ACL 环境下完成。
- 指纹短语由 Agent 证书 SHA-256 派生（5 个词），用户可肉眼核对。
- `PairRequest` 只在“配对模式”接受；配对模式只持续 5 分钟。
- 同一时间只允许 1 个配对请求；失败 5 次锁定 15 分钟。
- 配对成功后立即退出配对模式。

### 8.3 证书固定与轮换

- 客户端保存 Agent SPKI pin（当前 + 下一把，支持轮换）。
- Agent 轮换证书时，先在已认证通道发送 `NewPin`，客户端确认后切换；旧 pin 保留 30 天。
- v1 可先不做轮换：证书有效期 3 年，到期重新配对；但预留 `NewPin` 消息。
- Tailscale 名称/IP 变化时只重签服务端证书、保持同一密钥对，客户端 pin 的 SPKI 不变。
- 客户端证书吊销：从 `clients.json` 删除指纹；已有连接不断，下一次握手拒绝。

### 8.4 Keychain 存储

- CA 私钥：`SecItemAdd`，`kSecClassKey`，`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`，不 iCloud 同步。
- 客户端私钥在 Windows：DPAPI/Credential Manager（见 design.md）。
- Keychain 不可用时：不静默降级到明文文件；提示用户修复钥匙串，或使用 0600 文件 + 明确警告（仅开发模式）。

### 8.5 限流

| 动作 | 限制 |
|---|---|
| 配对尝试 | 5 次 / 10 分钟，失败锁定 15 分钟 |
| TLS 握手 | 10 次 / 分钟 / IP |
| 未认证连接 | 30 秒内必须完成握手 |
| 控制消息 | 100 条 / 秒 |
| 剪贴板 | 1 MB / 条，10 条 / 分钟 |

---

## 9. 视频采集（SCK）

### 9.1 内容选择

- 启动时 `SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true)`。
- 默认选主显示器（`CGMainDisplayID`）；配置可指定 `display_id`。
- `SCContentFilter(display: display, excludingWindows: [])`。
- 排除自身窗口（如果有设置窗口）避免递归。

### 9.2 配置

```rust
let mut cfg = SCStreamConfiguration::new();
cfg.width = target_width;          // 像素
cfg.height = target_height;
cfg.minimumFrameInterval = CMTime::new(1, fps as i32);
cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange; // 420v
cfg.scalesToFit = true;
cfg.preserveAspectRatio = true;
cfg.showsCursor = true;
cfg.queueDepth = 3;
cfg.colorSpaceName = kCGColorSpaceSRGB;
cfg.capturesAudio = false;          // 音频单独走 Tap
```

- 目标分辨率：默认按客户端请求（1080p/1440p/4K）；保持宽高比，取偶数。
- Retina：`SCContentFilter.pointPixelScale`（macOS 14.0+）把点换算为像素；下采样在 SCK 内完成（GPU），避免客户端再缩放。
- 像素格式优先 `420v`（NV12 视频范围），VT 可直接吃；如果某些 Mac 不支持，回退 `32BGRA` 并让 VT 转换。
- `showsCursor = true`：光标由 SCK 合成，客户端不必再画；B1 仍由 VNC 管光标。

### 9.3 回调

```rust
fn stream(&self, _stream: &SCStream, did_output: CMSampleBuffer, of: SCStreamOutputType)
```

- 只处理 `.screen`。
- 读取附件：
  - `SCStreamFrameInfo.status`：`.complete` 才处理；`.idle`/`.blank` 丢弃。
  - `SCStreamFrameInfo.displayTime`：作为 PTS。
  - `SCStreamFrameInfo.contentRect` / `.contentScale`：用于坐标映射和多屏。
- 取 `CVPixelBuffer`，检查 `IOSurface` 存在；包装为 `Frame`（持有引用），入编码队列。
- `autoreleasepool` 包裹回调；不在此做色彩转换/编码。

### 9.4 显示器变化

- 监听 `CGDisplayRegisterReconfigurationCallback` + `NSApplication.didChangeScreenParametersNotification`。
- 防抖 500 ms：分辨率/排列/主屏变化才重建，避免合盖/切换动画触发多次。
- 重建流程：停止旧 `SCStream` → 重新取 `SCShareableContent` → 新 filter/config → 新 stream → 新 VT session → 发 `DisplayChanged` + 关键帧。
- 重建期间客户端保留最后一帧，不黑屏。

### 9.5 多显示器与虚拟显示器

- v1 只采一个显示器（默认主屏）；配置可切换。
- 若客户端要求“全部显示器”，B2 需要多路 SCStream 或一个跨屏 framebuffer；M5 再评估，v1 不做。
- 无头 Mac：SCK 在无物理显示器时可能没有 `SCDisplay`；需要虚拟显示器（dummy plug 或 `CGVirtualDisplay` 私有 API 不可用）。M5 评估公开可行方案，v1 文档建议接 HDMI 假负载。
- HiDPI：按像素采集，客户端按 DPI 显示；不要在 Agent 做 UI 缩放。

### 9.6 失败与重建

- `SCStreamDelegate.stream(_:didStopWithError:)`：记录错误码，退避 0.5/1/2/5 s 重建，最多 10 次。
- 权限被撤销：`SCStreamError.userDeclined` 等；暂停并在菜单提示，不反复弹窗。
- `SCShareableContent` 为空：等待 1 s 重试；持续为空则提示“没有可捕获的显示器”。

---

## 10. 视频编码（VideoToolbox）

### 10.1 管线

```text
SCK CVPixelBuffer (IOSurface, 420v)
        │  直送（尽量零拷贝）
        ▼
VTCompressionSessionEncodeFrame
        │  CMSampleBuffer (AVCC)
        ▼
Annex-B 转换（SPS/PPS + start code）
        │
        ▼
有界发送队列 ──► QUIC video stream
```

- 编码在专用串行队列；不在 SCK 回调里同步编码。
- 同一时刻最多 2 帧在飞（`MaxFrameDelayCount = 1` + 队列 2）。
- 编码失败/分辨率变化：`VTCompressionSessionInvalidate`，重建，强制关键帧。

### 10.2 编码参数

| 属性 | 值 | 说明 |
|---|---|---|
| 编码器 | H.264（默认）/ HEVC（可选） | H.264 兼容性最好 |
| 硬件加速 | 允许；失败回退软件 | Apple Silicon 硬编 |
| `RealTime` | true | 实时 |
| `AllowFrameReordering` | false | 无 B 帧 |
| `MaxFrameDelayCount` | 1 | 低延迟 |
| `ProfileLevel` | H264_High_AutoLevel | 1080p60/4K 自动 |
| `AverageBitRate` | 客户端设定（2–50 Mbps） | 默认 8 Mbps @1080p30 |
| `DataRateLimits` | `[bitrate/8*1.5, 1.0]` | 1 秒窗口 |
| `MaxKeyFrameInterval` | fps × 2 | 2 秒一个 IDR |
| `MaxKeyFrameIntervalDuration` | 2 | 同上 |
| `ExpectedFrameRate` | fps | 帮助码控 |
| `PrioritizeEncodingSpeedOverQuality` | true | 低延迟优先 |
| `EnableLowLatencyRateControl` | 可用则 true | 无限 GOP、无 B/无 lookahead、High profile |

> 低延迟码控会带来“无限 GOP”，丢包后可能一直花屏；因此仍保留**按需强制关键帧**（重连、丢帧、客户端请求）。若实测强制 IDR 与低延迟模式冲突，则关闭低延迟码控，退回 2 秒 GOP。

### 10.3 关键帧与参数集

- IDR 前附 SPS/PPS（Annex-B）；SPS/PPS 来自 `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`。
- 每个关键帧都带参数集，客户端可在任意关键帧重建解码器，不依赖带外配置。
- 客户端请求 `RequestKeyframe` 时，下一帧强制 IDR（`kVTEncodeFrameOptionKey_ForceKeyFrame`）。
- 首帧、分辨率变化、编码器重建、重连后必须 IDR。

### 10.4 码率与自适应

- 客户端通过 `SetBitrate` 调整；Agent 设置 `AverageBitRate`/`DataRateLimits`，不重建会话。
- 帧率调整：重建 `SCStreamConfiguration.minimumFrameInterval`（轻量）或暂停/恢复。
- 分辨率调整：重建 SCK + VT（重）；v1 只允许在会话开始时协商，运行中不动态改分辨率。
- 弱网：客户端降码率 → Agent 降码率；仍不足则客户端请求降帧率/分辨率（重连）。
- 码率下限 1 Mbps（防糊），上限 50 Mbps（4K60）。

### 10.5 H.264 vs HEVC

- 默认 H.264：Windows Media Foundation 普遍支持，无需额外解码器。
- HEVC：Apple Silicon 硬编效率更高，但 Windows 需要 HEVC 视频扩展（部分机器没有）；仅在客户端明确支持且用户开启时使用。
- v1 只做 H.264；HEVC 放到 M5b，作为“带宽减半”选项。

### 10.6 错误处理

| 错误 | 处理 |
|---|---|
| `kVTInvalidSessionErr` | 重建 session + 关键帧 |
| `kVTVideoEncoderNotAvailableNowErr` | 退避 1 s 重试；连续失败回退软件编码 |
| 编码输出为空/超时 | 丢帧 + 计数；连续 3 次重建 |
| 码率设置失败 | 记录并保持旧值；不中断会话 |
| 内存压力 | 降分辨率/帧率；释放帧池 |

---

## 11. 音频采集

### 11.1 Core Audio Tap（首选，14.2+）

```text
CATapDescription(stereoGlobalTapButExcludeProcesses: [selfProcessObjectID])
        │
AudioHardwareCreateProcessTap
        │
私有 aggregate device（tap list）
        │
AudioDeviceCreateIOProcIDWithBlock
        │  Float32, 48 kHz, 2ch（或设备原生格式）
        ▼
rtrb 环形缓冲 ──► 重采样（如需要）──► Opus
```

- 只采系统输出混音；传入自身进程的 `AudioObjectID` 排除自身音频（避免把提示音回传）。
- 需要 `kTCCServiceAudioCapture` + `NSAudioCaptureUsageDescription`。
- 首次创建 tap 触发系统弹窗；拒绝时返回权限错误，菜单提示。
- 设备切换/默认输出变化：监听 `kAudioHardwarePropertyDefaultOutputDevice`，重建 tap/aggregate（避免静默）。

### 11.2 SCK audio 回退（13+，但需要屏幕录制权限）

- `SCStreamConfiguration.capturesAudio = true`、`sampleRate = 48000`、`channelCount = 2`、`excludesCurrentProcessAudio = true`。
- 与 B2 视频共用 SCStream 时，音频输出类型 `.audio`；B1 单独建一个只采音频的 SCStream。
- 缺点：需要屏幕录制权限；B1 若不想给屏幕权限，优先 Tap，Tap 不可用则禁用音频（而不是强要屏幕权限）。

### 11.3 格式与重采样

- 目标：48 kHz、立体声、Float32 interleaved。
- Tap 可能给设备原生采样率（44.1/48 kHz）；用 `rubato`（sinc）在编码线程重采样，不在 IOProc 里做。
- 单声道设备：复制到双声道；多声道：下混为立体声。
- 采样格式：Float32 → `opus_encode_float`；若拿到 Int16，直接 `opus_encode`。

### 11.4 实时安全

- IOProc 回调里**只做**：取 `AudioBufferList` → 写 `rtrb` → 返回。
- 不分配、不加锁、不打日志、不调用 ObjC。
- 环形缓冲 1 秒；满时丢最旧并计数（`audio_overflow`）。
- Opus 编码在专用线程，20 ms 一帧（960 samples）。

### 11.5 设备与权限变化

- 默认输出设备变化：重建 tap；重建期间静音 100–300 ms，客户端抖动缓冲吸收。
- 采样率变化：重建重采样器。
- 权限撤销：`AudioHardwareCreateProcessTap` 返回权限错误；停止音频，菜单提示。
- 系统静音：仍采集（音量 0），由客户端决定是否显示；不自动跳过。

---

## 12. 音频编码与传输

### 12.1 Opus 参数

| 参数 | 值 |
|---|---|
| 采样率 | 48000 |
| 声道 | 2 |
| 帧长 | 20 ms（960 samples） |
| 码率 | 64 kbps（语音/通用），96–128 kbps（音乐） |
| 模式 | VBR |
| 复杂度 | 5（默认），可调 3–10 |
| 带内 FEC | 开 |
| DTX | 关（桌面音频连续） |
| 丢包率提示 | 5% |
| 信号 | auto（音乐场景可设 music） |

### 12.2 打包与 FEC

- 每包：`seq`（u16，回绕）、`pts_us`（u32，回绕约 71 分钟）、`flags`、Opus payload；客户端按模 2^32 处理回绕。
- 包大小约 160 字节 @64 kbps/20 ms；远小于 1200 MTU。
- 带内 FEC：客户端在丢包时用下一包的 FEC 恢复；仍失败则 PLC。
- 不做重传：音频过期即弃，重传只会增加延迟。

### 12.3 时间戳与时钟

- 采集时刻用 `AudioTimeStamp.mHostTime` 转 `mach_absolute_time` → 微秒。
- 视频 PTS 同源（SCK `displayTime` 基于 host clock）。
- B2 音画同步：客户端用同一 `host_time` 基准做唇音同步（目标 ≤ 80 ms）。
- B1：视频来自 VNC，无共享时钟；音频自由播放，客户端只保证抖动缓冲，不做严格唇音同步（目标 ≤ 150 ms）。

### 12.4 客户端契约

- 抖动缓冲 40–120 ms 自适应；目标 60 ms。
- 丢包 > 10% 时优先保音频（降低视频码率）。
- 音频设备切换不重建会话；客户端本地切换输出。
- 静音/音量由客户端控制；Agent 不改变 Mac 音量（避免影响本地用户）。

---

## 13. 输入注入（B2）

### 13.1 键位映射

- 客户端发送 **macOS 虚拟键码**（`kVK_*`），而不是 X11 keysym；Agent 直接 `CGEventCreateKeyboardEvent`。
- Windows VK → macOS keycode 的映射表由客户端维护；Agent 只做校验与注入。
- 修饰键：`CGEventSetFlags` 设置 Command/Shift/Control/Option；同时按需发送修饰键 down/up，兼容只监听 keyDown 的应用。
- 布局差异：默认按物理位置（ANSI）；提供“按字符”模式，走 Unicode 注入。
- 未知键码：丢弃并计数，不崩溃。

### 13.2 鼠标与滚轮

- 坐标：客户端发送归一化坐标（0.0–1.0）+ 显示器 ID；Agent 映射到该显示器的全局坐标（points）。
- 多显示器：全局坐标原点在主屏左上角，其他屏可能为负；用 `CGDisplayBounds` 计算。
- 事件：move/down/up/drag/right/middle/other；`CGEventCreateMouseEvent` + `CGEventPost(kCGHIDEventTap)`。
- 滚轮：`CGEventCreateScrollWheelEvent`，`kCGScrollEventUnitPixel`，支持横滚（wheel2）。
- 双击/拖拽：客户端负责时序；Agent 不合并。

### 13.3 Unicode / IME

- 文本输入：`CGEventKeyboardSetUnicodeString` + keycode 0 的 down/up；适用于大部分应用。
- 中文/emoji：优先 Unicode 注入；失败时回退“写剪贴板 + Cmd+V”（需要剪贴板权限，且会改变远端剪贴板，需提示）。
- 远端输入法：Unicode 注入绕过输入法，直接上屏；适合“客户端输入法”场景。
- 复杂组合键（Cmd+Tab、Ctrl+Cmd+Q）走 keycode + flags，不走文本注入。

### 13.4 辅助功能权限

- 启动时 `AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt: true])`。
- 未授权：B2 输入禁用，视频/音频正常；菜单显示“需要辅助功能”。
- 授权后无需重启，下一次事件生效。
- 不申请“输入监控”（我们只注入，不监听）。

### 13.5 限制

- **登录窗口**：Agent 未运行（用户会话未建立），B2 无法注入；用 B1/RFB。
- **Secure Input**：密码框/部分安全应用会阻止合成事件；`IsSecureEventInputEnabled()` 为真时提示用户“该窗口可能无法输入”。
- **全屏安全应用**、系统保护窗口：可能拒绝 CGEvent；不尝试绕过。
- **用户切换**：会话非活跃时暂停注入。

---

## 14. 剪贴板

### 14.1 同步机制

- 轮询 `NSPasteboard.general.changeCount`，500 ms 一次（macOS 无变更通知）。
- 变化且非自身写入：读取 `public.utf8-plain-text` / `NSStringPboardType`，UTF-8 编码，发送 `ClipboardText`。
- 收到远端文本：`clearContents` → `setString:forType:` → 记录新的 `changeCount` 与内容哈希。
- 去重：`last_sent_hash` / `last_received_hash` + `changeCount`；避免 A→B→A 回环。
- 限长 1 MB；超长截断并提示（或拒绝，按配置）。

### 14.2 隐私

- 默认开启，但提供“仅手动同步”（客户端点“粘贴”才拉取）。
- 锁屏/用户切换时暂停。
- 疑似密钥/令牌格式（可选的正则提示）不自动同步，仅提示。
- 不记录剪贴板内容到日志。
- macOS 15+ 可能对程序化读取剪贴板弹提示；失败时回退手动模式，不反复弹窗。

### 14.3 类型

- v1 只做纯文本；图片/文件/富文本 P2。
- 远端写入时清空旧类型，避免残留 HTML/RTF 造成粘贴异常。

---

## 15. 生命周期与稳定性

### 15.1 看门狗

每 2 秒检查：

| 指标 | 阈值 | 动作 |
|---|---|---|
| 视频最后帧时间 | > 3 s（有客户端、屏幕未睡） | 重启 SCK/VT |
| 音频最后包时间 | > 1 s（有客户端） | 重启 Tap/Opus |
| 网络最后收包 | > 15 s | 关闭连接，等待重连 |
| 编码队列深度 | > 5 持续 5 s | 降帧率/码率，丢帧 |
| 内存 | > 600 MB | 释放帧池、降分辨率、记录 |
| 线程阻塞 | 控制消息 > 5 s 未处理 | 记录堆栈，重启会话 |

### 15.2 系统事件

| 事件 | 处理 |
|---|---|
| 睡眠 | 停止 SCK/Tap/编码，关闭 QUIC；不阻止睡眠（除非用户开启“保持唤醒”） |
| 唤醒 | 延迟 1 s 重建；等网络可用；客户端重连 |
| 锁屏 | 暂停视频采集（隐私）；音频按配置继续/暂停；恢复时关键帧 |
| 解锁 | 重建/恢复，发关键帧 |
| 用户切换 | 会话失活 → 暂停采集/注入；重新激活 → 恢复 |
| 显示器变化 | 防抖 500 ms，重建 SCK/VT |
| 默认音频设备变化 | 重建 Tap |
| 网络地址变化 | 重建 listener；已有连接不受影响 |
| 权限撤销 | 暂停对应能力，菜单提示 |

### 15.3 故障矩阵

| 故障 | 检测 | 恢复 | 用户感知 |
|---|---|---|---|
| SCK 流停止 | delegate error | 重建，退避 | 画面短暂冻结 |
| 无帧 | watchdog | 重建 | 同上 |
| VT 会话失效 | OSStatus | 重建 + 关键帧 | 短暂花屏/冻结 |
| 编码器不可用 | OSStatus | 软件编码/降级 | 码率下降 |
| Tap 创建失败 | OSStatus | SCK audio / 禁用 | 音频不可用提示 |
| 网络断开 | QUIC idle | 等待重连 | 状态橙色 |
| 配对失败 | 限流 | 重新生成码 | 提示 |
| 配置损坏 | 解析失败 | 备份 + 默认值 | 菜单提示 |
| 磁盘满 | 写日志失败 | 停止文件日志，仅 OSLog | 无感 |

### 15.4 资源上限

- 最大客户端数：1（v1）；多余连接拒绝并计数。
- 帧池：4 个 4K 缓冲（约 100 MB）上限；按当前分辨率动态分配。
- 队列：视频 2 帧、音频 200 ms、控制 32 条、剪贴板 1 条（覆盖）。
- 日志：单文件 10 MB × 3；OSLog 为主。
- 崩溃转储：最多 5 份，单份 5 MB。

### 15.5 电源与散热

- 无客户端时：停止采集/编码，CPU < 1%。
- 连接中：默认不阻止睡眠；用户可开“远程会话期间保持唤醒”（`IOPMAssertionCreateWithName`，`PreventUserIdleSystemSleep`）。
- 电池供电：可选择降帧率/码率（默认关闭，避免画质突变）。
- `ProcessInfo.thermalState` 为 `.serious`/`.critical`：降 fps/码率，记录。
- 合盖：外接显示器模式才可采集；否则提示。

### 15.6 配置

```toml
[agent]
version = 1
mode = "auto"            # auto | b1 | b2
max_clients = 1

[network]
bind = "auto"
port = 8443
enable_tcp_fallback = true

[video]
codec = "h264"           # h264 | hevc
fps = 30
bitrate_mbps = 8
resolution = "1080p"     # 1080p | 1440p | 4k | native
display_id = 0           # 0 = main

[audio]
enabled = true
bitrate_kbps = 64
capture = "auto"         # auto | tap | sck

[clipboard]
enabled = true
mode = "auto"            # auto | manual
max_bytes = 1048576

[security]
require_client_cert = true
pairing_ttl_sec = 300
```

- 原子写：临时文件 + `rename`；损坏时保留 `.bak` 并回退默认。
- 密钥/证书不写配置文件，只存 Keychain。
- 配置变更：菜单/客户端触发 reload；网络端口变化需重启 listener。

---

## 16. 性能设计

### 16.1 零拷贝与内存

- SCK 输出 `CVPixelBuffer`（IOSurface）直接交给 VT，不经过 `CGImage`/`NSImage`；VT 内部若复制，由硬件/驱动完成，不在 CPU 侧再转一次。
- 编码输出 `CMSampleBuffer` 只做一次 Annex-B 转换（必要，因为 Windows MF 兼容性最好）；转换缓冲复用。
- 音频 IOProc → `rtrb` → Opus，无中间 Vec 分配（预分配）。
- 帧池按分辨率复用；不每帧 `malloc`。
- ObjC 回调包 `autoreleasepool`，避免临时对象堆积。

### 16.2 预算

| 场景 | CPU（M1） | 内存 | 延迟 |
|---|---|---|---|
| B1 音频 + 剪贴板 | < 5% | < 80 MB | 音频 60–100 ms |
| B2 1080p30 H.264 | 10–20% | 150–250 MB | 40–80 ms |
| B2 1080p60 H.264 | 15–25% | 150–300 MB | 40–80 ms |
| B2 4K30 H.264 | 30–50% | 300–500 MB | 50–100 ms |
| B2 4K60 HEVC | 40–60% | 400–600 MB | 60–120 ms |

> 数字为目标值，需在 M1/M2/M3/Intel 上实测校准；重点是“不超过预算”而不是追极限。

### 16.3 延迟预算（B2，LAN）

| 阶段 | 预算 |
|---|---|
| SCK 帧间隔 | 16–33 ms |
| VT 编码 | 5–15 ms |
| 队列/发送 | 1–5 ms |
| 网络（Tailscale 直连） | 1–10 ms |
| 客户端解码 | 5–10 ms |
| 客户端呈现 | 16 ms |
| **合计** | **50–90 ms** |

### 16.4 测量

- `os_signpost` 标记 capture/encode/send 区间；Instruments 可视化。
- 指标：`capture_fps`、`encode_ms_p50/p95`、`send_queue`、`dropped_frames`、`bitrate`、`audio_underruns`、`rtt`、`thermal_state`。
- 每次发布跑固定场景：静态桌面、滚动网页、播放视频、拖窗、4K 时间线。

---

## 17. 安全设计

### 17.1 威胁模型（LAN/Tailscale）

| 资产 | 威胁 | 控制 |
|---|---|---|
| 屏幕/音频 | Tailnet 内恶意设备、ARP/局域网嗅探 | TLS 1.3 + mTLS；绑定 Tailscale；ACL |
| 配对/证书 | 中间人、暴力破解 | 指纹短语、配对码、限流、pinning |
| 输入 | 恶意客户端注入 | 证书白名单、单客户端、审计 |
| 剪贴板 | 敏感信息泄露 | 限长、手动模式、锁屏暂停 |
| 更新 | 恶意替换 bundle | Developer ID 签名 + 公证 + EdDSA 清单 |
| 本地数据 | 密钥被窃 | Keychain、FileVault、0600 配置 |
| 被控端 | 隐蔽控制 | 菜单栏常驻指示、连接日志、可一键断开 |

### 17.2 最小权限

- 不 root、不 LaunchDaemon、不 Full Disk Access、不输入监控。
- B1 只申请系统音频；B2 才申请屏幕录制 + 辅助功能。
- 权限状态可视化；撤销后立即停止对应能力。

### 17.3 传输与认证

- TLS 1.3 only；禁用弱套件/压缩/0-RTT。
- 服务端证书自签 + 客户端 SPKI pin；客户端证书 mTLS。
- 配对模式限时 5 分钟；失败锁定。
- 绑定 Tailscale/LAN 地址；不监听公网；建议 Tailscale ACL 限制 8443。

### 17.4 审计与隐私

- 连接日志：时间、设备名、证书指纹、模式、时长、字节数；保留 7 天。
- 不记录屏幕内容、按键、剪贴板文本、音频。
- 诊断包脱敏：版本、指标、错误码、权限状态；不含内容。
- 用户可一键清空日志/撤销所有客户端。

### 17.5 更新

- 更新包必须签名（Developer ID + 公证）；应用内清单 Ed25519 签名 + SHA-256。
- 降级保护：拒绝低于当前版本的包，除非用户显式操作。
- 更新失败回滚：保留旧 bundle，启动失败自动回退（见 §18）。

---

## 18. 安装、签名、更新、卸载

### 18.1 安装包

- 组件：`RustVNC Agent.app` + 安装脚本（可选）。
- 安装位置：`/Applications`（推荐）；需要管理员权限一次。
- 备选：`~/Applications`（免管理员），需验证 `SMAppService.agent` 在该路径的行为；M4 决定。
- pkg 使用 `productbuild`；签名 `productsign`。

### 18.2 签名与公证

- Developer ID Application 证书；hardened runtime；`--timestamp`。
- 先签内部二进制/框架，再签 app；避免 `--deep`。
- `notarytool submit --wait`；`stapler staple`。
- Entitlements：仅必要项；不启用 sandbox（屏幕采集/辅助功能工具通常不适合 App Store）。
- App Store：不做；说明原因（TCC/LaunchAgent/辅助功能限制）。

### 18.3 更新

**首选**：Sparkle 2（EdDSA 签名、成熟、支持菜单栏应用）；通过薄 ObjC 桥接入。
**备选**：自研更新器：

1. 拉取签名清单（Ed25519）。
2. 下载 zip/pkg，校验 SHA-256 + 签名。
3. 解压到临时目录，校验 bundle 签名（`SecStaticCodeCheckValidity`）。
4. 退出 Agent；用 helper 原子替换 `/Applications/RustVNC Agent.app`（旧 bundle 改名备份）。
5. 重新启动；失败则回滚旧 bundle。

- 自动检查频率：每天一次；可关闭。
- 更新前提示“会短暂断开连接”。
- 替换 bundle 可能影响 TCC（同一 Team ID/Bundle ID 通常保留）；更新后自检权限，缺失则引导。

### 18.4 卸载

- 菜单“卸载” → `SMAppService.unregister` → 删除 app（需要权限）→ 可选删除 `~/Library/Application Support/RustVNC`、`~/Library/Logs/RustVNC`、Keychain 项。
- 提供 uninstall 脚本/说明；不留下 LaunchAgent。
- 卸载前断开客户端，避免“幽灵连接”。

---

## 19. 可观测性

### 19.1 指标

| 指标 | 类型 | 用途 |
|---|---|---|
| `capture_fps` | gauge | 采集是否正常 |
| `encode_ms_p50/p95` | histogram | 编码瓶颈 |
| `send_queue_frames` | gauge | 网络背压 |
| `dropped_frames` | counter | 画质/性能 |
| `bitrate_bps` | gauge | 实际码率 |
| `audio_underruns` | counter | 抖动/卡顿 |
| `audio_overflow` | counter | IOProc 堆积 |
| `rtt_ms` | gauge | 网络 |
| `reconnects` | counter | 稳定性 |
| `thermal_state` | gauge | 降频 |
| `permission_state` | gauge | 权限 |
| `restart_count` | counter | 组件健康 |

### 19.2 日志

- `os_log`（subsystem `com.rustvnc.agent`，category `capture/encode/audio/net/auth`）。
- 默认 `info`；诊断模式 `debug`（5 分钟自动关闭）。
- 文件日志可选：`~/Library/Logs/RustVNC/agent.log`，轮转 10 MB × 3。
- 脱敏：设备名可保留；IP 可保留；证书指纹保留；不记录内容。

### 19.3 诊断包

一键导出 zip：版本、macOS 版本、硬件、配置（脱敏）、最近 10 分钟日志、指标快照、权限状态、网络接口（Tailscale/LAN 是否存在）。不含屏幕/音频/剪贴板/按键。

### 19.4 崩溃

- 默认由 macOS 生成 `.ips`；Agent 启动时收集最近崩溃摘要到日志。
- 可选 opt-in 上传（默认关闭）；上传前展示内容。
- 崩溃循环：launchd `ThrottleInterval` 防抖；连续 3 次崩溃进入“安全模式”（不启用 B2，仅 B1）。

---

## 20. 测试计划

### 20.1 矩阵

| 维度 | 取值 |
|---|---|
| macOS | 14.0、14.2、14.6、15.x、26.x、27.x |
| 硬件 | M1、M2/M3、M4（如有）、Intel Mac mini |
| 显示 | 内置 Retina、5K、双 4K、无头 + dummy plug、合盖外接 |
| 音频 | 内置扬声器、USB 声卡、蓝牙、HDMI、44.1/48 kHz、静音 |
| 网络 | 千兆 LAN、Wi-Fi 6、Tailscale 直连、Tailscale DERP、1–5% 丢包、20–100 ms 延迟 |
| 权限 | 全授予、拒绝、运行中撤销、升级后保留/丢失 |
| 客户端 | Windows 10/11 x64/arm64，B1/B2/auto |

### 20.2 功能验收

- B1：音频端到端 ≤ 100 ms；8 小时无断流/爆音；剪贴板双向 ≤ 300 ms；1 MB 文本不崩溃；RFB 画面不受影响。
- B2：1080p30 稳定 30 fps；1080p60 ≥ 55 fps；首帧 ≤ 1.5 s；断网 3 s 内恢复；分辨率切换 ≤ 2 s；输入延迟 P95 ≤ 80 ms（LAN）。
- 配对：首次 ≤ 30 s；错误码/限流正确；重启后免配对。
- 权限：拒绝时不崩溃、提示可操作；授予后无需重启。
- 睡眠/唤醒/锁屏/用户切换：自动恢复，无花屏/无声。

### 20.3 性能与稳定性

- 8 小时 soak（B1/B2）；24 小时可选。
- 1000 次重连；100 次分辨率切换；50 次睡眠/唤醒。
- 内存增长 < 5%；FD 数量稳定；线程数稳定。
- 4K60 30 分钟，观察 thermal/帧率。

### 20.4 故障注入

| 注入 | 期望 |
|---|---|
| 杀 SCK 流 | 3 s 内重建，画面恢复 |
| VT 返回 invalid session | 重建 + 关键帧 |
| 拔网线/切 Wi-Fi | 重连，音频/视频恢复 |
| 撤销屏幕录制 | 暂停 B2，提示；B1 不受影响 |
| 撤销辅助功能 | 输入停止，视频继续 |
| 默认音频设备切换 | 100–300 ms 静音后恢复 |
| 磁盘满 | 文件日志停止，OSLog 继续 |
| 配置损坏 | 回退默认，保留 .bak |
| CPU 100%（stress） | 降帧率，不崩溃 |

### 20.5 安全测试

- TLS 扫描：仅 1.3、无弱套件、无压缩。
- 证书固定：错误证书/错误 pin 必须拒绝。
- 配对爆破：限流生效；日志记录。
- 未授权客户端：mTLS 拒绝。
- 控制消息 fuzz：不 panic、不 OOM。
- 剪贴板注入：超长/二进制/UTF-8 边界安全。
- 更新包：篡改签名必须拒绝。

---

## 21. 里程碑与工作量

### 21.1 阶段

| 阶段 | 周期 | 内容 | 出口 |
|---|---|---|---|
| **A0** | 3 天 | app bundle、菜单栏、launchd、单实例、配置 | 安装后能常驻菜单栏 |
| **A1** | 1 周 | QUIC/TLS、配对、控制协议、客户端联调 | 客户端能连上并完成配对 |
| **A2** | 1 周 | 音频（Tap/SCK）、Opus、剪贴板、权限引导 | B1 音频/剪贴板可用 |
| **A3** | 1 周 | 看门狗、睡眠/锁屏、更新、soak | B1 Beta 稳定 |
| **A4** | 2 周 | SCK 视频、VT、CGEvent、B2 会话 | B2 1080p30 可用 |
| **A5** | 1 周 | 1080p60/4K、码率自适应、HEVC 评估、虚拟显示 | B2 性能达标 |
| **A6** | 1 周 | 兼容矩阵、安全测试、发布 | GA |

- **B1 可独立交付**：A0–A3 完成后即可发布“无高画质但音频/剪贴板可用”的版本；B2 是风险最高的部分，必要时可裁剪到 M6 之后。
- A0–A3 = 4 周，对应 `design.md` 的 **M4**。
- A4–A5 = 3 周，对应 **M5**。
- A6 与 **M6** 合并。

### 21.2 人力

- macOS/Rust 工程师 1.0（主线）
- Rust/网络工程师 0.5（QUIC/协议/客户端联调）
- QA/发布 0.3（真机矩阵、签名/公证）
- 合计约 1.8 FTE × 7 周 ≈ 12–13 人周。

### 21.3 依赖与并行

- A1 依赖客户端 Agent 客户端模块（可先用 `quinn` 示例/CLI 联调）。
- A2 依赖权限引导设计（UI 文案可先占位）。
- A4 依赖 B1 稳定；B2 失败可随时回退 B1。
- 签名/公证在 A0 就配置好，避免末期卡壳。

---

## 22. 风险与开放问题

### 22.1 风险

| # | 风险 | 概率 | 影响 | 应对 |
|---|---|---|---|---|
| R1 | TCC 归属/LaunchAgent 导致权限异常 | 中 | 高 | A0 先验证；SMAppService + 签名 bundle；矩阵测试 |
| R2 | Core Audio Tap 在 14.2/14.4/15 行为差异 | 高 | 中 | 版本分支 + SCK 回退；真机矩阵 |
| R3 | Tailscale MTU/QUIC 分片 | 中 | 中 | 1200 载荷；抓包验证；TCP 回退 |
| R4 | CGEvent 在 Secure Input/登录窗口失效 | 高 | 中 | B1 覆盖登录；UI 提示；不绕过 |
| R5 | VT 画质/延迟不达预期 | 中 | 中 | 参数调优；H.264 baseline 回退；实测 |
| R6 | 更新替换 bundle 导致 TCC 重置 | 中 | 高 | 同 Team ID/Bundle ID；更新后自检；文档 |
| R7 | macOS 26/27 API 变更 | 中 | 高 | 每版回归；SCK 优先；保留 B1 |
| R8 | 无头 Mac 无 SCDisplay | 中 | 中 | dummy plug/虚拟显示；文档 |
| R9 | 音频设备切换爆音/断流 | 中 | 低 | 抖动缓冲；重建淡入淡出 |
| R10 | 公证/签名失败延误发布 | 低 | 高 | A0 配置流水线；预留缓冲 |

### 22.2 开放问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| Q1 | B2 是否进 v1？ | 范围/工期 | 先 B1，B2 作为 M5 可选 |
| Q2 | 默认 H.264 还是 HEVC？ | 兼容/带宽 | H.264 默认，HEVC 可选 |
| Q3 | 音频默认 Tap 还是 SCK？ | 权限/稳定性 | 14.2+ Tap；否则 SCK/禁用 |
| Q4 | 更新用 Sparkle 还是自研？ | 工期/稳定 | Sparkle 优先，M4 评估 |
| Q5 | 安装到 `/Applications` 还是 `~/Applications`？ | 权限/更新 | `/Applications` + pkg；免管理员方案 M4 验证 |
| Q6 | 是否支持多客户端？ | 复杂度 | v1 单客户端 |
| Q7 | 是否需要麦克风上行？ | 范围 | v2 |
| Q8 | 是否接入 Tailscale 身份（LocalAPI）？ | 安全/复杂度 | P2，先证书配对 |

---

## 23. 附录

### A. 关键 API 与属性

| 用途 | API/属性 |
|---|---|
| 屏幕内容 | `SCShareableContent`、`SCContentFilter`、`SCStream` |
| 流配置 | `SCStreamConfiguration`（`width/height/minimumFrameInterval/pixelFormat/queueDepth/capturesAudio/sampleRate/channelCount`） |
| 帧状态 | `SCStreamFrameInfo.status/displayTime/contentRect/contentScale` |
| 系统音频 | `CATapDescription`、`AudioHardwareCreateProcessTap`、aggregate device + IOProc |
| 编码 | `VTCompressionSessionCreate`、`VTCompressionSessionEncodeFrame`、`CMSampleBuffer` |
| 低延迟 | `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`、`kVTCompressionPropertyKey_EnableLowLatencyRateControl` |
| 输入 | `CGEventCreateKeyboardEvent`、`CGEventCreateMouseEvent`、`CGEventPost` |
| 辅助功能 | `AXIsProcessTrustedWithOptions` |
| 剪贴板 | `NSPasteboard.general`、`changeCount` |
| 登录项 | `SMAppService.agent(plistName:)` |
| 显示器 | `CGDisplayRegisterReconfigurationCallback`、`CGDisplayBounds` |
| 电源 | `IOPMAssertionCreateWithName`、`ProcessInfo.thermalState` |

### B. 编码参数速查

```text
H.264 High AutoLevel
RealTime = true
AllowFrameReordering = false
MaxFrameDelayCount = 1
AverageBitRate = 8_000_000
DataRateLimits = [1_500_000, 1.0]
MaxKeyFrameInterval = 60
MaxKeyFrameIntervalDuration = 2
ExpectedFrameRate = 30
PrioritizeEncodingSpeedOverQuality = true
EnableLowLatencyRateControl = true (若支持)
```

### C. 协议消息

| 消息 | 通道 | 方向 | 说明 |
|---|---|---|---|
| `Hello` | control | C→S | 协议版本、能力 |
| `ServerHello` | control | S→C | Agent 版本、能力、显示器 |
| `PairRequest` | control | C→S | 配对码、CSR、设备名 |
| `PairResponse` | control | S→C | 客户端证书、CA |
| `StartSession` | control | C→S | 模式、音频/剪贴板/视频配置 |
| `SetBitrate` | control | C→S | 码率 |
| `SetFps` | control | C→S | 帧率 |
| `RequestKeyframe` | control | C→S | 强制 IDR |
| `VideoFrame` | video | S→C | Annex-B 帧 |
| `AudioPacket` | datagram | S→C | Opus 包 |
| `Input` | input | C→S | 键鼠/文本 |
| `ClipboardText` | clipboard | 双向 | UTF-8 文本 |
| `Stats` | control | S→C | 指标 |
| `Ping/Pong` | control | 双向 | RTT |

### D. 权限检查清单

- [ ] `NSAudioCaptureUsageDescription` 已写入 Info.plist
- [ ] 首次音频采集触发系统弹窗，拒绝时给出可操作提示
- [ ] 屏幕录制权限只在 B2 请求，B1 不请求
- [ ] 辅助功能权限只在 B2 请求，未授权时输入禁用
- [ ] 权限撤销后停止对应能力，不崩溃
- [ ] 更新后自检权限，缺失时引导
- [ ] 菜单栏/系统指示器可见
- [ ] 不申请 root/Full Disk Access/输入监控

### E. 参考资料

1. [ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
2. [SCStreamConfiguration](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration)
3. [Capturing screen content in macOS](https://developer.apple.com/documentation/screencapturekit/capturing-screen-content-in-macos)
4. [VideoToolbox compression properties](https://developer.apple.com/documentation/videotoolbox/compression-properties)
5. [Capturing system audio with Core Audio taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps)
6. [CATapDescription](https://developer.apple.com/documentation/coreaudio/catapdescription)
7. [NSAudioCaptureUsageDescription](https://developer.apple.com/documentation/bundleresources/information-property-list/nsaudiocaptureusagedescription)
8. [SMAppService](https://developer.apple.com/documentation/servicemanagement/smappservice)
9. [Tailscale MTU 说明](https://tailscale.com/docs/reference/troubleshooting/network-configuration/tcp-connection-two-devices)
10. [RustDesk（AGPL-3.0，仅架构参考）](https://github.com/rustdesk/rustdesk)
11. [RustDesk scap（MIT）](https://github.com/rustdesk/rustdesk/tree/master/libs/scrap)
12. [RustDesk enigo（MIT）](https://github.com/rustdesk/rustdesk/tree/master/libs/enigo)
13. [quinn（QUIC）](https://crates.io/crates/quinn)
14. [rustls（TLS）](https://crates.io/crates/rustls)
15. [opus](https://crates.io/crates/opus)

---

## 24. 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| v0.1 | 2026-11-16 | 初稿：B1/B2 双模、Tailscale 绑定、SCK+VT、Core Audio Tap、配对/权限/稳定性/测试与 7 周计划 |
| v0.2 | 2026-11-16 | A1 控制面落地（QUIC/TLS 1.3、配对、mTLS、Ping/Pong）；A1 简化项见 README-A1.md |
| v0.3 | 2026-11-16 | A1 安全收口：CSR、Keychain、按 IP 限流、自动化集成测试 |
