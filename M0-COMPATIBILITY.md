# M0 兼容性报告 v0

> 日期：2026-11-16 · 环境：macOS 27.2.1（26B131）· Apple Silicon（arm64）
> 目标：证明"能连上、能出画、能输入"，并记录 macOS Screen Sharing 的协议差异。

## 1. 结论摘要

| 项 | 结果 |
|---|---|
| RFB 握手 / 版本串 | ✅ 实测 macOS 声明 `RFB 003.889`，客户端回 `003.008` 可用 |
| 安全类型探测 | ✅ 实测 `[30, 33, 36, 31, 32, 2, 35]`；ARD 与 VNC 密码同时可用 |
| Apple ARD 认证流程 | ✅ 走到服务端 SecurityResult，错误密码返回 `Authentication or authorization failure` |
| VNC 密码认证流程 | ✅ 错误密码返回 `Wrong password` |
| 正确认证 + 真机出画 | ⏳ 需要 macOS 账号/VNC 密码；本环境不持有凭据 |
| Raw 帧 + 键鼠输入 | ✅ mock RFB 服务器端到端通过（VNC DES 认证、64×64 帧、Pointer/Key 事件） |
| PNG 截图 | ✅ mock 会话保存 PNG |
| ZRLE / Tight / 16bpp | ⏳ 代码路径已接入 vnc-rs，真机需认证后验证 |
| Apple 1104/1105/1109 | ❌ vnc-rs 未实现；记为 M1/M2 缺口 |
| 剪贴板 | ✅ 已由 Agent 通道验证：RFB 5900 不承载剪贴板 |

**判定**：M0 的协议可行性与"能连上、能输入"已验证；真机正确认证与 ZRLE 出画仍缺凭据，保留为 **条件通过（conditional pass）**。

## 2. 实测握手

```bash
cargo run -p rfb-probe -- --inspect --host 127.0.0.1 --port 5900
```

输出：

```text
server_version: RFB 003.889
security_types: [30, 33, 36, 31, 32, 2, 35]
  30: Apple ARD (type 30)
  33: Apple legacy SRP/DH
  36: Apple SRP
  31: unknown/Apple private
  32: unknown/Apple private
  2: VNC authentication (DES)
  35: Apple legacy SRP/DH
```

说明：

- macOS 声明 `003.889`，标准客户端应回 `003.008`；`vnc-rs` 在启用 ARD 凭据时会自动这样做。
- `--inspect` 只做版本/安全类型探测，不提交凭据。
- 该 Mac 同时启用了 ARD（30）与 VNC 密码（2）；实际使用优先 ARD。

## 3. 认证失败路径

```bash
VNC_PASSWORD=definitely-wrong cargo run -p rfb-probe -- \
  --host 127.0.0.1 --user "$USER" --seconds 1

VNC_PASSWORD=wrong cargo run -p rfb-probe -- \
  --host 127.0.0.1 --auth vnc --seconds 1
```

结果：

```text
Apple Remote Desktop authentication failed: Authentication or authorization failure
Wrong password
```

说明握手、安全类型选择、Apple DH 交换、VNC DES challenge 都已到达服务端；失败发生在服务端授权判断。

## 4. 成功认证 / 出画（待凭据）

在持有凭据的机器上执行：

```bash
read -r VNC_USERNAME          # macOS 短用户名（whoami）
read -s VNC_PASSWORD          # 账号密码
export VNC_USERNAME VNC_PASSWORD

cargo run -p rfb-probe -- \
  --host 127.0.0.1 \
  --port 5900 \
  --seconds 10 \
  --save /tmp/rfb-frame.png \
  --input-test
```

期望：

- `CONNECTED`，打印 `server_name`、分辨率、像素格式。
- `STATS ... rects>0`，生成 `/tmp/rfb-frame.png`。
- `--input-test` 会在远端当前焦点窗口输入一个 `A`，并点击屏幕中心；请只在可接受输入的会话中使用。

`--auth vnc` 可强制走 VNC 密码（8 字符 DES）路径；`--auth ard` 强制走账号认证。

## 5. Mock 端到端验证

`crates/rfb-probe/tests/mock_vnc.rs` 启动一个最小 RFB 3.8 服务器：

- 只提供 security type 2（VNC DES）
- 固定 challenge/response（密码 `secret`）
- ServerInit 64×64 BGRA
- 收到 FramebufferUpdateRequest 后发送 Raw 帧
- 记录 Pointer/Key 事件

测试命令：

```bash
cargo test -p rfb-probe
```

覆盖：

- VNC DES 认证成功
- Raw 帧解码与 PNG 保存
- PointerEvent / KeyEvent 发送
- 客户端在 1 秒后正常退出

## 6. 协议/实现缺口

| 缺口 | 影响 | 计划 |
|---|---|---|
| vnc-rs 无 zlib(6)/Hextile(5) | 只能请求 ZRLE/Tight/Raw | ZRLE 已覆盖主要压缩；zlib 作为 M1 备选 |
| vnc-rs 无 Apple 1104/1105/1107/1109/1110 | 光标/缩放/键盘布局信息缺失 | M1/M2 在 fork 中补 `rfb-apple` |
| Tight 是否被 macOS 支持未验证 | 影响 JPEG 质量旋钮 | 认证成功后以 `--encoding tight` 验证 |
| 16bpp 真机行为未验证 | 影响带宽档位 | 认证成功后以 `--bpp 16` 验证 |
| 无真机成功帧 | 无法做逐像素比对 | 由持有凭据的用户执行第 4 节 |
| 无 Windows 构建验证 | 客户端最终目标平台 | CI 已加 `windows-latest` 的 probe 构建/测试 |
| `ringbuf 0.4.8` 安全公告（RUSTSEC-2026-0293） | flexaudio 依赖；仅在 f32 采样上使用，Drop panic 路径不可达 | 已记录 ignore 理由；上游升级后移除 |
| `time 0.3.45` 安全公告（RUSTSEC-2026-0009） | 解析恶意 RFC2822 输入可栈溢出 | 已升级到 0.3.47；MSRV 调整为 1.88 |

## 7. 复用评估

**结论：M0 继续使用 `vnc-rs 0.6.1` 作为协议基线，暂不 fork。**

理由：

- 已内置 `RFB 003.889` 处理、Apple ARD（type 30）、VNC DES、ZRLE/Tight/Raw/CopyRect、Cursor/DesktopSize。
- mock 与真机握手/认证失败路径均通过。
- 缺口集中在 Apple 私有编码与请求调度，可在 M1 以 `rfb-apple` 外层或最小补丁补齐；当前 fork 成本高于收益。

后续触发 fork 的条件：

- 需要 1104/1105 光标/显示几何；或
- 需要自定义 16bpp/请求节流；或
- 上游 API 无法暴露解码后的帧/输入队列。

## 8. 复现环境

| 项 | 值 |
|---|---|
| OS | macOS 27.2.1 (26B131) |
| CPU | Apple Silicon arm64 |
| RFB | 003.889 |
| Screen Sharing | 已启用（5900 可连） |
| Remote Management | 已启用；`VNCLegacyConnectionsEnabled=1` |
| Rust | 1.100.0 |
| Probe | `rfb-probe 0.1.0`（vnc-rs 0.6.1） |
| CI | macOS：fmt/clippy/test/release；Windows：probe；`cargo deny check` |

## 9. 下一步（M1）

1. 用真实凭据完成 ARD/VNC 成功出画，补 PNG 与逐像素比对。
2. 验证 ZRLE/Tight 16/32bpp、增量更新粒度、无头 Mac 分辨率。
3. 在 `vnc-rs` 外层补 Apple 1104/1105/1109 解析与多显示器信息。
4. 把 probe 的帧循环抽成 Windows 客户端可复用的 `session`/`render` 层。
