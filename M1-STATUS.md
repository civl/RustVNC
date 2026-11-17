# M1 / M1b 状态报告 — 可用客户端

> 日期：2026-11-17 · 环境：Windows 11 x64 / macOS 构建在 CI · Rust 1.100.0
> 目标：日常可用的 VNC 客户端，覆盖 80% 个人用户场景。
>
> **本版按 [BLOCKER-ANALYSIS.md](BLOCKER-ANALYSIS.md) 的修复顺序做了 1–6 项，并把
> 原先「条件通过」里过度声明的条目改回事实。** 变化见 §0。

## 0. 修复轮（2026-11-17）

| 原卡点 | 现在 | 证据 |
|---|---|---|
| 空闲连接烧 100% 单核 | `poll_event` → `recv_event`，100% → **0.000s / 3s** | `tests/idle_cost.rs`（CI 每轮跑） |
| 未知编码直接断线并每秒重连 | zlib(6) 真正解码；Apple 1104/1105/1107/1109/1110 按 u16 长度安全跳过；其余编码返回 `UnsupportedEncoding(n)` 且**不可重试** | `tests/encoding_compat.rs`（3 例） |
| Ctrl 被同时当成 Cmd+Ctrl | 用 `mac_cmd` 而非 `command`；修饰键改为状态机（Ctrl+点击、Shift+拖拽可用）；新增特殊键菜单 | `main.rs` 单测 4 例 |
| 连接无超时、`close()` 卡 UI | 连接 10s / 握手 15s 超时；`close()` 不 join；短会话不重置退避 | `session.rs` |
| SSH 首次连接必然失败、错误不可见 | 指纹探测 + 显式信任写入 known_hosts；ssh stderr 透出并带建议；可一键生成密钥 | `ssh.rs` 单测 6 例 |
| 双引擎只有 Mac 半边 | 新增 `crates/agent-client`：配对(mTLS+指纹校验)、剪贴板双向、Opus→WASAPI 音频 | `tests/agent_loopback.rs`（真实 QUIC/TLS 回环） |
| Agent 装不上 | entitlements + hardened runtime + `notarize.sh`；Windows `sign.ps1`（缺证书时可强制失败）；`.gitattributes` 防 CRLF | 脚本 + CI |

## 1. 结论

客户端核心、配置/凭据、输入、SSH 隧道、UI 骨架与 Agent 增强通道均已落地，并通过自动化测试；
**真机连接已在 Mac mini M4（macOS 27.2，Tailscale 组网）上打通**，剩下的是需要人工操作的输入
与时延测量：

| M1 验收项 | 状态 |
|---|---|
| 真机出画 | ✅ 5120×2880 / ZRLE / 57.31 fps（8 s）；1080p 90 s soak 56.65 fps、0 断线 |
| 16 bpp / zlib(6) 真机 | ✅ 16bpp 61–62 fps 颜色正确；zlib 解出 5K 全屏帧像素正确 |
| Tight | ❌ macOS 不认，回退 Raw（印证设计判断，默认不请求） |
| 登录窗口画面 | ✅ 实测捕获到锁屏（含「你的屏幕正在被观察」提示） |
| LAN 1080p ≥ 20 fps | ✅ 实测 56.65 fps（1080p）、57.31 fps（5K） |
| 1080p 连续 30 分钟不崩溃 | ⏳ 已做 90 秒 soak；30 分钟需你按 ACCEPTANCE.md 跑 |
| 输入 P95 ≤ 120 ms | ⏳ 未做（本次刻意不做输入注入，避免干扰你正在用的 Mac） |
| 登录窗口可输入 | ⏳ 需你在注销后自测 |
| 窗口变化 ≤ 1 s 重绘 | ⏳ 需人工观察（分辨率变化路径已有代码） |
| SSH 主机密钥变更拒绝 | ✅ `StrictHostKeyChecking=yes`，未信任时在 spawn 前拒绝（单测） |
| 未知编码安全跳过 | ✅ zlib 真机通；Apple 私有编码真机通；未知编码报编码号且不重连 |
| 连接 CRUD / 搜索 / 导入导出 | ✅ |
| 凭据保存 / 不保存 | ✅ Keychain/Credential Manager + 内存回退 |
| 5 档画质 + 自定义 | ✅（切换需重连） |
| 剪贴板（macOS 方向） | ✅ 真机验证：Mac 上复制 → Windows 收到（纯 RFB 对 macOS 无效，协议里就没有） |
| 系统音频 | ✅ 真机验证：Mac 播放音乐 → 50 包/秒 Opus → Windows WASAPI 出声 |

**判定：功能面完成，真机出画/fps 已验证**；输入与时延、30 分钟稳定性按 ACCEPTANCE.md 自测即可闭环。


## 2. M1b 增补

- **自动重连**：1/2/4/8/15 秒指数退避；认证/配置错误不重试；用户断开立即停止；
  只有稳定 10 秒以上的会话才重置退避（避免「连上就断」的 1 秒重连风暴）。
- **统一错误码**：`AUTH` / `NETWORK` / `PROTOCOL` / `CONFIG` / `SSH` / `INTERNAL`。
- **诊断日志**：stderr + 每日滚动文件；诊断包导出（不含密码）。
- **局部纹理更新**：Raw/CopyRect 只上传变化区域。
- **输入**：修饰键状态机 + 物理键位 + 按键重复 + 特殊键菜单（⌘Tab / ⌘Space / ⌃⌘Q / ⌘⌥Esc …）。
- **剪贴板**：本地 → RFB `ClientCutText`（第三方服务端）**和** Agent 通道（macOS）；
  Agent 发来的文本会写回本地并去重，避免回环。
- **音频**：Agent 数据报 → Opus 解码 → 48 kHz 立体声 WASAPI；先攒 60 ms 再播放，欠载输出静音。

## 3. 已实现（架构）

### 客户端会话

- `crates/rustvnc-client`，协议引擎为 **vendored `vnc-rs 0.6.1` + 补丁**（`vendor/vnc-rs/PATCHES.md`）。
- 异步 session actor：连接、刷新、Raw/Copy 帧、剪贴板文本、统计；事件经通道投递，UI 不阻塞网络线程。
- 支持 32bpp BGRA / 16bpp RGB565；ZRLE / zlib / Tight / Raw；未知编码明确报错。

### 增强通道（新）

- `crates/agent-client`：QUIC + TLS 1.3 + mTLS，ALPN `rustvnc-agent/1`。
- 配对：本地生成密钥 → 只发 CSR → Agent 签发客户端证书；同时校验
  **Agent 声明的指纹 == TLS 实际出示的证书指纹**，阻断「声明一个、出示另一个」的 MITM。
- 控制面：`Hello/ServerHello`、`Ping/Pong` 保活、`ClipboardText` 双向、`Goodbye`。
- 音频：QUIC datagram → Opus 解码 → 无锁环形缓冲 → cpal/WASAPI。

### 安全

- 默认推荐 SSH；host key 必须已固定，客户端提供指纹探测 + 显式信任，**绝不 `accept-new`**。
- 明文直连仍可用（Tailscale 等场景），但 UI 标注来源。
- 凭据：Keychain / Credential Manager；`SshConfig::password` 与 `AgentConfig` 的配对码都不落盘。

## 4. 验证

```bash
# Windows 能跑的全部
cargo test -p rustvnc-client -p rustvnc-agent-client -p rfb-probe -p vnc-rs
cargo clippy -p rustvnc-client -p rustvnc-agent-client -p rfb-probe -p vnc-rs --all-targets -- -D warnings

# macOS（含 Agent）
cargo test --workspace
./scripts/build-app.sh release
```

当前计数（2026-11-17）：

| 套件 | 数量 | 覆盖 |
|---|---|---|
| `rustvnc-client` 单测 | 14 | 配置映射、凭据、字体、输入、SSH |
| `rustvnc-client` 二进制单测 | 4 | 修饰键转换/顺序/幂等 |
| `tests/encoding_compat.rs` | 3 | zlib 解码、Apple 跳过、未知编码不重连 |
| `tests/idle_cost.rs` | 1 | 空闲 CPU 回归 |
| `tests/session_mock.rs` | 2 | 会话帧/输入、自动重连 |
| `rustvnc-agent-client` 单测 | 8 | 协议布局、TLS 地址解析、状态目录、Opus 解码 |
| `tests/agent_loopback.rs` | 3 | 配对+剪贴板、错配对码、音频数据报 |
| `vnc-rs`（vendor） | 60 | 上游套件 + zlib 共享流 |

## 5. 未完成 / 已知缺口

| 缺口 | 说明 | 计划 |
|---|---|---|
| 真机指标 | 帧率/延迟/30 分钟稳定性/登录窗口 | **需要你的 Mac + Windows 真机**，见 ACCEPTANCE.md |
| Apple 私有编码内容 | 只做安全跳过；1105 缩放、1109 布局未解析 | 抓到真机数据后解析 |
| 远端光标形状 | ✅ 32bpp 下渲染远端形状（热点+掩码已处理）；16bpp 不请求光标编码，仍用本地指针 | 16bpp 需服务端按 16bpp 发送光标，未验证 |
| SSH 密码认证（Windows） | 系统 OpenSSH 无 askpass；已明确报错 + 引导密钥 | 可选：换 `russh` 进程内实现 |
| 多显示器 / HiDPI | 未实现 | M2 |
| 自适应画质 | 快环已做：静止画面自动降到 4 fps（`FramePacer`），一有像素立刻恢复设定帧率，HUD 显示「静止降频」；慢环（按带宽/丢包调画质）仍是 M2 | 慢环 M2 |
| 渲染后端 | OpenGL(glow)，非 D3D11；无 WARP 回退 | M2 |
| 真机 E2E CI | 需要自托管 Mac runner | M6 |

## 6. 如何完成最终验收

见 [ACCEPTANCE.md](ACCEPTANCE.md)：里面有逐项操作、期望结果、记录表格，以及
`scripts/acceptance.ps1` 一键跑完所有能自动化的部分。

## 7. 下一步

1. 按 ACCEPTANCE.md 在真机上跑一遍，把 6 个 ⏳ 变成实测数字。
2. 抓 1104/1105/1109/1110 的真实字节（`--apple` 开关 + 诊断日志），补光标与 Retina 映射。
3. D3D11 渲染 + 自适应控制（M2）。
