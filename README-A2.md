# RustVNC Agent — A2 菜单栏服务 + 剪贴板

> A2b 已加入 Core Audio Tap + Opus 音频，见 [README-A2B.md](README-A2B.md)。

A2 把 A1 的控制服务接进菜单栏进程，并加上双向文本剪贴板：

- Agent 启动时自动启动 QUIC 控制服务（默认 `0.0.0.0:8443`）
- 菜单栏显示：服务器地址/状态、配对码、客户端连接状态
- 配对码一键复制；配对成功后自动标记“已使用”
- 菜单可启动/停止服务器；停止后重新启动会生成新配对码
- 文本剪贴板双向同步（Agent ↔ 客户端）
- 开发用环境变量：`RUSTVNC_LISTEN`、`RUSTVNC_PAIR_CODE`、`RUSTVNC_AGENT_DIR`、`RUSTVNC_NO_KEYCHAIN`

音频/视频/键鼠仍属于后续阶段。

## 1. 菜单栏

启动 `RustVNC Agent.app` 后：

| 菜单项 | 说明 |
|---|---|
| 服务器：运行中 0.0.0.0:8443 | 当前监听地址；启动失败时显示错误 |
| 配对码：12345678（点击复制） | 8 位一次性配对码，点击写入剪贴板 |
| 客户端：未连接 / xxx（已连接） | 配对与连接状态 |
| 启动/停止服务器 | 停止会终止当前会话；再启动生成新配对码 |
| 登录项：已启用 | `SMAppService` 状态 |
| 启用/停用登录时启动 | 注册/注销 LaunchAgent |
| 打开登录项设置… | 跳转系统设置 |
| 打开日志目录 | `~/Library/Logs/RustVNC/` |
| 退出 | 停止服务器并退出（launchd 不会复活） |

服务器默认监听 **所有接口的 8443/UDP**。传输仍是 TLS 1.3 + mTLS；如果只希望走 Tailscale，可设置：

```bash
RUSTVNC_LISTEN=100.x.y.z:8443
```

（launchd 启动时可通过 `launchctl setenv` 或修改 LaunchAgent plist 注入环境变量。）

## 2. 剪贴板

- 只同步 **UTF-8 文本**，上限 1 MiB；图片/文件/富文本不在 A2 范围。
- **连接建立后只同步变化**：连接前的剪贴板内容会作为基线，不会自动上传。
- 双向：
  - Agent 剪贴板变化 → `ClipboardText` → 客户端
  - 客户端 `ClipboardText` → Agent 写入 `NSPasteboard`
- 客户端会打印收到的文本长度；测试时可发送指定文本：

```bash
target/debug/rvncctl ping \
  --addr 127.0.0.1:8443 \
  --count 1 \
  --state-dir /tmp/rvnc-a2-client \
  --no-keychain \
  --clipboard-text "hello from client"
```

然后在 Mac 上 `pbpaste` 验证。

- 实现使用 `/usr/bin/pbpaste` / `/usr/bin/pbcopy` 子进程，避免在 tokio 线程直接调用 AppKit。
- 轮询间隔 1 秒；每次轮询会短暂 fork/exec，首包 RTT 可能多出十几毫秒，之后恢复。
- 如果 `pbpaste` 失败（例如权限/系统策略），该会话会禁用剪贴板并记录警告，不影响 Ping/控制面。
- macOS 15+ 可能对程序化读取剪贴板弹权限提示；按系统提示授权即可。

## 3. 开发/测试用环境变量

| 变量 | 作用 |
|---|---|
| `RUSTVNC_LISTEN` | 覆盖菜单栏服务器的监听地址，如 `127.0.0.1:18448` |
| `RUSTVNC_PAIR_CODE` | 固定配对码，便于自动化测试 |
| `RUSTVNC_AGENT_DIR` | 覆盖 Agent 状态/证书目录 |
| `RUSTVNC_NO_KEYCHAIN` | 设置后私钥写 0600 文件，不碰 Keychain |

示例：不经过 launchd 直接跑一个隔离实例：

```bash
RUSTVNC_LISTEN=127.0.0.1:18448 \
RUSTVNC_PAIR_CODE=12345678 \
RUSTVNC_AGENT_DIR=/tmp/rvnc-gui-agent \
RUSTVNC_NO_KEYCHAIN=1 \
"dist/RustVNC Agent.app/Contents/MacOS/rustvnc-agent"
```

## 4. 已验证

- 菜单栏启动时自动启动 QUIC 服务，`lsof -iUDP:18448` 可见监听。
- 通过菜单栏进程完成 CSR 配对 + Ping（RTT 0.2–0.6 ms，排除剪贴板轮询干扰）。
- Agent → 客户端：连接后修改 Mac 剪贴板，客户端收到 `ClipboardText`。
- 客户端 → Agent：`--clipboard-text` 发送后，Mac `pbpaste` 读到相同文本。
- 连接前的剪贴板内容不会自动发往客户端。
- `cargo test`：5 个单元测试 + 5 个集成测试全部通过。
- `cargo clippy --all-targets -D warnings`：通过。

## 5. 已知限制

- 还没有音频/视频/键鼠；`CAPABILITIES` 仍为 `0x0`。
- 剪贴板没有菜单开关，默认开启；后续加入“暂停/仅手动”策略。
- 使用 `pbpaste`/`pbcopy` 子进程，轮询有少量开销；后续可换成 AppKit `NSPasteboard` 专用线程。
- 菜单 UI 只做基础状态展示，没有权限向导/码率设置。
- 服务器默认监听所有接口；生产环境建议用 `RUSTVNC_LISTEN` 限定到 Tailscale 地址或配合 macOS 防火墙。

## 6. 代码位置

```text
src/main.rs        # 菜单栏、服务器生命周期、配对码/状态、启动停止
src/clipboard.rs   # pbpaste/pbcopy 封装与大小限制
src/server.rs      # 服务器事件、认证会话、双向剪贴板循环
src/client.rs      # 客户端收发 ClipboardText
tests/             # 控制面集成测试
```
