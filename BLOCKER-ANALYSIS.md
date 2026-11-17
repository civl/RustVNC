# RustVNC 卡点分析（2026-11-17）

> 分析对象：`civl/RustVNC` @ `fe7555a`（origin/main，2026-11-16 22:55 推送，12 个提交）
> 分析环境：Windows 11 / Rust 1.100.0 / 代码 5,740 行（agent 2,602 + client 2,291 + probe 686 + 集成测试 161）
> 分析方式：克隆全量历史 → 通读设计与状态文档 → 逐 crate 读关键路径 → 构建与测试 → 两个针对性复现实验

---

## 修复结果（2026-11-17，同日完成 1–6，7 待真机）

| # | 卡点 | 状态 | 落地位置 / 证据 |
|---|---|---|---|
| P0-1 | 空闲烧 100% 单核 | ✅ 已修 | `session.rs` 改用 `recv_event()`；`tests/idle_cost.rs` 实测 **0.000s / 3s**（CI 每轮跑） |
| P0-2 | 输入：Ctrl 变 ⌃⌘、Cmd 发不出、修饰键不连续 | ✅ 已修 | `ModifierState` 状态机 + `mac_cmd`；特殊键菜单；4 个单测 |
| P0-3 | 未知编码致命 + 缺 zlib/Apple 编码 | ✅ 已修 | `vendor/vnc-rs`（见其 `PATCHES.md`）；`tests/encoding_compat.rs` 3 例 |
| P0-4 | SSH 首次信任、密码、stderr | ✅ 已修 | 指纹探测+显式信任、stderr 透出、密钥生成；密码通过 `SSH_ASKPASS` 指向客户端自身（Unix 8.4+ / Windows 8.6+） |
| P0-5 | 双引擎客户端侧 0 行 | ✅ 已实现 | 新增 `crates/agent-client`（配对/剪贴板/Opus→WASAPI）；`tests/agent_loopback.rs` 真实 QUIC 回环，本机实测出声 |
| P0-6 | Agent 签名 / 仓库卫生 | ✅ 已修 | `resources/RustVNCAgent.entitlements`、`scripts/build-app.sh`（hardened runtime）、`scripts/notarize.sh`、`packaging/windows/sign.ps1`、`.gitattributes`、CI 拆分 |
| P0-7 | 无法验收 | ⚠️ 部分 | 自动化靶场 ✅（`scripts/acceptance.ps1`，本次全部 PASSED）；真机 6 项仍待你执行，清单见 `ACCEPTANCE.md` |

> 仍未解决的一项：Apple 私有编码的**内容**解析（1104/1105 需真机抓包；1109/1110 已解析）。
> 真机输入/时延与 10/30 分钟 soak 仍按 ACCEPTANCE 人工执行。

---

## 真机验证（2026-11-17 补测，Mac mini M4 / macOS 27.2 / Tailscale）

用你提供的 Mac 做了真实连接，这批数据把上文所有「待真机」的假设变成了事实，也**抓出了
一个会挡住所有 5K/6K Mac 的硬 bug**。

| 项目 | 实测 |
|---|---|
| 握手 | `RFB 003.889`，安全类型 `[30, 33, 36, 31, 32, 2, 35]` |
| Apple ARD 账号认证 | ✅ 成功（`Apple ARD auth done`），无需 VNC 密码 |
| 首次连接 | ❌ 失败：`Image data cannot be decoded correctly` |
| 根因 | 5120×2880 = 14,745,600 像素 > 引擎写死的 `MAX_PIXELS = 8,294,400`（正好 4K） |
| 修复后 | ✅ 5120×2880 出画，8 秒 459 帧、57.31 fps，PNG 可读（菜单栏/日期/状态图标正确） |
| 1080p（显示器休眠后） | ✅ 90 秒 soak：5,100 帧、56.65 fps、0 错误、0 断线 |
| ZRLE | ✅ 主力编码 |
| **zlib(6)** | ✅ 真机可用：5 秒解出 2 个 5K 全屏帧（117.9 MB）像素正确 |
| Tight | ❌ macOS 不认：回退 Raw（5 秒 456 MB 解码量），印证设计里的判断 |
| 16 bpp | ✅ 工作（1080p，62.14 fps） |
| Raw | ✅ 工作（1080p，10 fps） |
| Apple 私有编码 | ✅ 请求后 1107/1109/1110 各来一次（详见下） |
| 端口 22 / 8443 | ❌ 未开放：SSH 隧道与 Agent 通道在这台 Mac 上无法实测 |

**Apple 私有编码实测字节**（这是 P0-3 里我说“必须真机抓包”的那一项）：

```text
1107  20B  00 01 00 04 | 10 08 fd 00 … fd 03          （能力列表，4 项）
1109  31B  00 01 00 00 00 00 00 17 | com.apple.keylayout.ABC
1110  45B  … 00 09 | Mac16,10\0 unknown\0 unknown\0
```

结论：`u16` 长度前缀假设**成立**，跳过逻辑没有造成失步（Apple 矩形之后会话继续跑到
51 fps）。1104 在指针不动时不发送；1105 单屏机器上没发。据此新增 `vendor/vnc-rs/src/apple.rs`，
用真实字节做单测，解析出键盘布局与机型号并写进诊断日志。

> 注意：`STATS` 里的 `bytes` 是**解码后**字节数，不是网络流量；带宽要用抓包或系统计数器。
> 本次未做输入注入（避免移动/点击你正在用的 Mac），因此「输入延迟 P95」「能输入」仍待你在
> 方便时按 ACCEPTANCE.md 第 3、7 节自测。

---


## 0. 本次实测得到的事实

| 检查项 | 结果 |
|---|---|
| `cargo test -p rustvnc-client` | ✅ 7 单测 + 2 mock 集成全绿（1.07s） |
| `cargo clippy -p rustvnc-client --all-targets -- -D warnings`（Windows） | ✅ 无告警 |
| `cargo check --workspace`（Windows） | ❌ `could not compile objc2`（根包是 macOS Agent） |
| 空闲连接的 CPU 占用 | ❌ **100% 单核**（4.02s CPU / 4.01s 墙钟，见 §2.1） |
| 收到 1104 / 1105 / 1109 / 1110 / 6 编码 | ❌ 会话**立刻断开**（见 §2.2） |
| 上游 `HsuJv/vnc-rs` main（2026-11-12） | 编码枚举与本仓 0.6.1 **完全相同**，无 Apple 扩展、无 zlib(6) |

代码质量本身没有明显问题：clippy 干净、错误码分层清晰、测试会跑。**卡点几乎全部在“集成”和“真实环境验证”两层，而不是“代码写不出来”。**

---

## 1. 卡点速览

| # | 卡点 | 严重度 | 阻塞什么 | 是否需要外部条件 |
|---|---|---|---|---|
| P0-1 | 会话线程忙等，空闲烧 100% 单核 | 致命（性能/耗电/笔记本） | 任何可发布版本 | 否，本地可修 |
| P0-2 | 非白名单编码＝致命错误；缺 zlib(6) 与全部 Apple 扩展；上游不接 | 致命（协议能力） | 真机出画质量、光标、带宽 | 需 vendor vnc-rs |
| P0-3 | **双引擎的客户端侧 0 行代码**：Windows 客户端连不上 Agent | 致命（方案 C 有一半没做） | 音频、剪贴板、TLS、配对 | 需新增 crate |
| P0-4 | Ctrl 被同时当成 Cmd+Ctrl 发送；Cmd 无法单独发送 | 高（输入不可用） | 快捷键、Ctrl+点击 | 否，本地可修 |
| P0-5 | 无连接超时；`close()` 会 join 卡死 UI；SSH 只有密钥且强制 known_hosts | 高（连不上/假死） | 首次连接成功率 | 否，本地可修 |
| P0-6 | 无真实 macOS 凭据/真机；Windows 无法构建整个 workspace | 高（无法验收） | 一切“条件通过”的指标 | **需要你提供** |
| P1-1 | Agent 是 ad-hoc 签名，无 entitlements / hardened runtime / 公证 | 高（分发） | 用户装不上、TCC 每次失效 | 需 Apple 开发者账号 |
| P1-2 | 渲染是 OpenGL(glow)，不是设计的 D3D11 | 中（兼容性） | RDP/虚拟机/无驱动机器 | 否 |
| P1-3 | 远端光标被丢弃；多显示器/HiDPI/自定义 fps 未落地 | 中（体验） | “日常可用” | 部分需真机 |

---

## 2. P0 详解

### 2.1 空闲连接烧 100% 单核（已复现）

**现象**：客户端连上服务器、画面静止、无输入时，会话线程把一个核跑满。

**实测**：mock RFB 服务器握手后只回空更新，会话空闲 6 秒，采样该进程 CPU：

```text
空闲 4 秒窗口内 CPU 时间 = 4.02s / 墙钟 4.01s  => 100% 单核
```

**根因**：`crates/rustvnc-client/src/session.rs:336` 的 `tokio::select!` 用了 `client.poll_event()`：

```rust
event = client.poll_event() => { ... }   // session.rs:354
```

而 vnc-rs 的 `poll_event` 是**非阻塞**实现（`vnc-rs/src/client/connection.rs:253`）：

```rust
async fn poll_event(&mut self) -> Result<Option<VncEvent>, VncError> {
    match self.output_ch.try_recv() {
        Err(TryRecvError::Empty) => Ok(None),   // 立刻返回
        ...
```

`select!` 的分支一旦 ready 就返回，于是循环以“纳秒级”空转；ticker 分支只是在空转里被随机抽中才偶尔执行。库本身提供了阻塞版 `recv_event()`（同文件 `:365`），这里用错了 API。

**修法**（1 行）：改用 `recv_event()`，并把 `ClientNotRunning` 当正常关闭处理：

```rust
event = client.recv_event() => match event {
    Ok(event) => handle(event),
    Err(vnc::VncError::ClientNotRunning) => break,   // 远端关闭 → 走重连
    Err(err) => return Err(SessionFailure::from_vnc(err).after_connect()),
}
```

> 附带影响：这个忙等还会和 `Refresh` 的 ticker 抢调度，导致请求节流精度变差——设计里“单请求在途 + 帧率上限”的限流实际上不可靠。

---

### 2.2 编码兼容：任何“不在白名单里”的编码都会杀死会话（已复现）

**实测**：mock 服务器在首个 `FramebufferUpdate` 里返回一个矩形，编码分别取 1104/1105/1109/1110/6：

```text
ENCODING  1104 -> Error code=PROTOCOL message=Image data cannot be decoded correctly (closed=true)
ENCODING  1105 -> Error code=PROTOCOL message=Image data cannot be decoded correctly (closed=true)
ENCODING  1109 -> Error code=PROTOCOL message=Image data cannot be decoded correctly (closed=true)
ENCODING  1110 -> Error code=PROTOCOL message=Image data cannot be decoded correctly (closed=true)
ENCODING     6 -> Error code=PROTOCOL message=Image data cannot be decoded correctly (closed=true)
```

**根因**：`vnc-rs` 的 `VncEncoding::from_wire()` 对未知值返回 `Err(InvalidImageData)`（`config.rs:29-42`），解码循环直接向上抛错；客户端把它映射成 `Protocol` 且 `retryable = true`（`session.rs:447-449`、`419-421`），而失败发生在握手之后 → `attempt` 每次被重置 → **默认配置下变成每 1 秒重连一次的死循环**，用户看到的是“连不上/一直闪断”。

**这不是一个可以绕过去的 bug，而是能力缺口**：

| 编码 | macOS 是否支持 | vnc-rs 0.6.1 | 本仓是否请求 | 后果 |
|---|---|---|---|---|
| ZRLE(16) | ✅ | ✅ | ✅ | 目前唯一主力 |
| CopyRect(1) / Raw(0) / -223 / -239 | ✅ | ✅ | ✅ | 基本可用 |
| **zlib(6)** | ✅ | ❌ **枚举里没有** | ❌ | 丢掉一大档压缩（设计 §5.1.3 明确要） |
| **1104 光标** | ✅ | ❌ | ❌ | 远端光标缺失，或光标被烧进画面（拖影+大区域重发） |
| **1105 显示几何** | ✅ | ❌ | ❌ | Retina 缩放因子/display id 拿不到 |
| **1109 键盘布局 / 1110 机型** | ✅ | ❌ | ❌ | 布局提示、机型号诊断都没有 |
| Tight(7) | 多数版本不支持 | ✅ | 可选 | 无 JPEG 旋钮 |

**关键约束**：`VncEncoding` 是 `#[repr(i32)]` 的**封闭枚举**，`from_wire` 是 `pub(crate)`，解码 match 在 `connection.rs` 内部——**在 crate 外部无法扩展**。上游 `HsuJv/vnc-rs` main 分支的枚举与 0.6.1 完全一致，也不打算加（开放 issue 5 个）。所以设计文档 §5.1.0 里“vendor 到 `crates/rfb`”这句必须真的执行，否则 M1/M2 的编码缺口一个都补不了。

**修法**：

```toml
# Cargo.toml
[patch.crates-io]
vnc-rs = { path = "vendor/vnc-rs" }
```

在 vendored 副本里做三件事：

1. 枚举补 `Zlib = 6`、`AppleCursor = 1104`、`AppleDisplay = 1105`、`AppleCaps = 1107`、`AppleKeyboard = 1109`、`AppleMachine = 1110`；
2. 解码循环加 `Apple*` 分支：读 `u16` 长度（上限 64 KiB）后 `read_exact` 丢弃，取出 1105 的 scale（f64）与 1109 的布局串做真实用途；
3. 补 zlib(6) 解码器：`flate2` 已经是 vnc-rs 的依赖，只需按“整条连接共用一个 zlib 流 + 每矩形 `u32` 压缩长度”实现（ZRLE 的解压代码可直接复用）；
4. 兜底：未知且非 Apple 的编码 → 明确返回 `UnsupportedEncoding(i32)`（带编码号），并在客户端映射成**不可重试**错误，避免重连死循环。

> 参考事实：Apple 自家客户端广告的编码是 `1011, 1002, 6, 16, -239, 1104, 1100, -223, 1101, 1105, 1107, 1109, 1110`；Apple 私有编码的 body 以 `u16` 长度开头，可以跳过（见 design.md §D 参考文献 3 的实测记录）。

---

### 2.3 双引擎只做了一半：Windows 客户端没有任何 Agent 通道代码

这是**文档与代码差距最大的一处**。

`crates/rustvnc-client/Cargo.toml` 的依赖里**没有** `quinn` / `rustls` / `opus` / `cpal` / `windows`：

```toml
[dependencies]
ab_glyph, anyhow, arboard, dirs, eframe, egui, keyring, serde, serde_json,
tokio, tracing, tracing-appender, tracing-subscriber, vnc-rs
```

也就是说：

| 能力 | Agent 侧（macOS，`src/`） | 客户端侧（Windows，`crates/rustvnc-client`） | 结论 |
|---|---|---|---|
| TLS/QUIC 控制面 | ✅ 2,602 行，`rvncctl` 可测 | ❌ 0 行 | Windows 客户端**拨不通** 8443 |
| 配对 / mTLS / 证书固定 | ✅ | ❌ 0 行 | 无法配对 |
| 剪贴板双向 | ✅ | 只走 `ClientCutText`/`ServerCutText` | macOS 的 RFB **不承载剪贴板**，实际无效 |
| 系统音频（Opus） | ✅ 采集+编码 | ❌ 无 Opus、无 WASAPI | 声音**永远到不了** Windows |
| 加密（除 SSH 外） | ✅ | ❌ | 只剩 SSH 或 Tailscale |

我在 `crates/rustvnc-client` 全目录搜索 `opus|audio|wasapi|cpal`，**零命中**。而 `M1-STATUS.md` 把“剪贴板双向”标为 ✅ 已完成、`README.md` 把 A2b 音频标为已完成——两条都只在 macOS 半边成立。

`agent-design.md` §4.1 已经把 B1 的连接模型写清楚了（RFB 5900 + Agent 8443 两条独立连接），缺的就是客户端这半边的实现：

```text
需要新增 crates/agent-client
  ├─ tls.rs       rustls + 证书固定（复用 src/tls.rs 的判定逻辑，去掉 danger 验证器）
  ├─ pairing.rs   配对码 → CSR → 客户端证书（与 src/server.rs 的协议对齐）
  ├─ control.rs   Hello/ServerHello/Ping/Pong（postcard 编解码与 src/protocol.rs 对齐）
  ├─ clipboard.rs 文本双向 + 去重 + 1 MiB 限长
  └─ audio.rs     QUIC datagram → 抖动缓冲 → opus 解码 → WASAPI（windows-rs 或 cpal）
```

粗估：控制面 + 剪贴板 3–5 天，音频链路（含抖动缓冲/设备热插拔）5–8 天。这是“方案 C”剩下最大的一块工作量，也是**它才是产品差异化的来源**（纯 RFB 永远没有音频和剪贴板）。

---

### 2.4 输入层：Ctrl 被当成“Cmd+Ctrl”发出去，Cmd 根本发不出来

`crates/rustvnc-client/src/main.rs:656-678`：

```rust
if modifiers.ctrl   { held.push(0xffe3); }   // Control_L
if modifiers.command{ held.push(0xffeb); }   // Super_L → macOS 的 Command
```

egui 里 `Modifiers::command` 的语义是“**Windows/Linux 上等于 ctrl**，macOS 上等于 mac_cmd”。因此在 Windows 上按 `Ctrl+C`：`ctrl == true && command == true` → 远端同时收到 `Control_L` + `Super_L` + `c`，macOS 侧看到的是 **⌃⌘C**，不是 ⌃C。所有 Ctrl 快捷键全错。

同时：

- `mac_cmd`（Windows 键）在代码里**完全没被使用**，egui 的 `Key` 也没有 Windows 键 —— 设计承诺的“Win → Command”在当前 API 下**无法实现**；
- 修饰键不是状态机，而是“每个按键事件前按下、事件后释放”（`main.rs:670-677`），所以 **Ctrl+点击多选、Shift+拖拽** 这类“修饰键跨越鼠标事件”的操作不成立；
- 没有“发送特殊键”菜单（`Cmd+Tab`、`Cmd+空格`、`Cmd+Option+Esc` 全部无法触发）。

**修法**：

```rust
if modifiers.ctrl    { held.push(0xffe3); }   // Ctrl
if modifiers.alt     { held.push(0xffe9); }   // Option
if modifiers.shift   { held.push(0xffe1); }
if modifiers.mac_cmd { held.push(0xffeb); }   // Windows 键 → Command（替代 command）
```

并把修饰键改成状态机：每帧比对 `input.modifiers` 与 `last_modifiers`，对**变化**的修饰键发送 down/up；鼠标事件前先确保远端修饰键状态与本地一致。此外给 Win 键单独做一个 UI 开关（因为 egui 拿不到该按键），或加一个“按住 Command”切换按钮。

---

### 2.5 连不上 / 假死：超时、阻塞 join、SSH 首次信任

1. **没有任何连接超时**。`session.rs:275` 的 `TcpStream::connect` 与 `try_start()` 的握手都没有 `tokio::time::timeout` 包裹。服务器若“接受 TCP 但不发版本串”（防火墙、错误端口、screensharingd 卡住），客户端会**无限等待**，UI 上只显示“连接中”，没有任何退路。

2. **`Session::close()` 会同步 `join` 会话线程**（`session.rs:179-184`）。配合第 1 点，用户点“断开”或关闭窗口时，**UI 线程会一起卡死**，直到那个连接自己超时（可能是几十秒到永不）。`Drop` 里也发了 Close，但 join 依然在。

3. **SSH 隧道对“第一次连一台新 Mac”100% 失败**（`ssh.rs:35-59`）：

   ```rust
   .arg("BatchMode=yes")                 // 只允许密钥，不弹密码
   .arg("StrictHostKeyChecking=yes")     // known_hosts 里没有就直接失败
   .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
   .stderr(Stdio::null())                // 失败原因被丢掉
   ```

   - `BatchMode=yes` → 密码/键盘交互认证不可用，**设计 FR-17 承诺的“密码/密钥”只实现了密钥**；
   - 新机器不在 `known_hosts` 里 → 立刻退出（`ssh exited before the tunnel was ready: exit status 255`），用户拿不到任何可用信息（stderr 被丢进 null）；
   - 设计 §5.2.2 承诺的“首次连接展示 host key 指纹、用户确认后固定”**没有实现**。

   对一个普通用户来说，默认推荐的安全模式（SSH）实际上是不可用的，最后只能退回明文直连——这与“默认安全”的产品承诺正面冲突。

**修法**：

- `connect`/`try_start` 包 `timeout(10s)`，超时映射成 `NETWORK` 错误并给出下一步建议；
- `close()` 改为发送关闭信号后**不 join**（线程自己退出），或 `join` 带超时；
- SSH：`AcceptNew` 首次连接先 `ssh-keyscan` 取指纹，在 UI 里让用户核对“SHA256 指纹 + 是否信任”，写入 known_hosts 后再用 `StrictHostKeyChecking=yes`；同时支持密码认证（`-o PreferredAuthentications=password` + `SSH_ASKPASS` 注入，或改用纯 Rust 的 `russh`）；
- 把 `ssh` 的 stderr 接出来，作为错误详情显示。

---

### 2.6 无法验收：`条件通过` 的上限

所有“⏳ 需真实凭据”的项目，本质是同一个卡点：**没有可用于端到端验证的 macOS 端**。叠加两件事：

- 根包 `rustvnc-agent` 是 macOS-only（`objc2`、`flexaudio-os-macos`、`security-framework`），在 Windows 上 `cargo check --workspace` 直接编译失败 → **在 Windows 开发机上无法验证 Agent 侧任何改动**，`cargo clippy --workspace` 也不可用（CI 只跑 `-p` 子集）；
- 没有 fuzz/proptest 目标（设计 §5.1.6、§8.2 要求），协议解析只靠 2 个 mock 用例覆盖。

**建议的破局顺序**（不需要 Mac 也能推进大半）：

1. 用 **Linux + TigerVNC / neatvnc** 当协议正确性靶场（ZRLE/zlib/CopyRect/未知编码/分辨率变化全部可控），先在 CI 里把“能出画、能输入、不崩、不烧 CPU”跑成回归；
2. 把 `tmp_blocker_probe.rs` 里那两个场景（未知编码、空闲 CPU）固化成**永久回归测试**；
3. macOS 真机只用于“兼容性矩阵”验证，不再作为所有里程碑的阻塞点。

真正需要你提供的只有三样：**一台可连的 Mac（开屏幕共享）+ 账号/VNC 密码 + 一台 Windows 真机**，用来跑设计里的 30 分钟稳定性 / fps / 输入 P95。

---

## 3. P1：分发与体验

### 3.1 Agent 装不上（P1-1）

`scripts/build-app.sh`：

```bash
IDENTITY="${CODESIGN_IDENTITY:--}"          # 默认 ad-hoc 签名
codesign --force --sign "$IDENTITY" ...      # 没有 --options runtime
```

- **没有 entitlements 文件**，没有 hardened runtime，没有 `notarytool` → 无法公证分发，用户双击会被 Gatekeeper 拦；
- 默认 `-`（ad-hoc）签名，而 **TCC 授权（System Audio Recording）与签名标识绑定**：每次重新构建签名都变 → 音频权限每次失效，用户要反复去系统设置里重勾；
- `SMAppService` 在 ad-hoc 签名下会返回 `kSMErrorInvalidSignature`（脚本自己注释里承认了）。

Windows 侧同样没有 Authenticode（`packaging/windows/build.ps1` 只调 Inno Setup），SmartScreen 必然拦截。

### 3.2 渲染后端与设计不符（P1-2）

设计 §5.3.1 选的是 D3D11 + flip model；实际 `eframe` 用的是 `glow`（OpenGL，`Cargo.toml` 的 `features = ["glow", ...]`）。后果：RDP 会话、虚拟机、无独显驱动的机器上 OpenGL 上下文可能创建失败，而设计里“WARP 软件回退”这条路不存在。要么补 D3D11/wgpu 后端，要么明确接受 OpenGL 并加上失败回退。

### 3.3 其他已知缺口（与文档一致或略超）

| 项 | 现状 | 备注 |
|---|---|---|
| 远端光标 | `VncEvent::SetCursor` 被直接丢弃（`session.rs:371`） | 与 P0-2 的 1104 一起修才有意义 |
| `Connected` 事件 | 上报 `width: 0, height: 0`（`session.rs:329`） | 分辨率靠后续 `Resized` 补，属于无害 bug |
| 自定义档位 | 只能改 bpp/编码，`fps()` 固定 30（`config.rs:152-156`） | 设计承诺 5–60 fps 可调 |
| 多显示器 / HiDPI | 未实现 | 设计 P0（FR-11/FR-12） |
| 请求节流 | 只有固定 ticker，没有“单请求在途”与自适应（§5.4 双环） | 与 2.1 的忙等叠加后限流不可靠 |
| `.gitattributes` | **不存在**，系统 `core.autocrlf=true` | Windows 上提交会把 `scripts/*.sh` 变成 CRLF，直接破坏 macOS 构建脚本（本次克隆已复现该行为） |

---

## 4. 文档与代码的不一致（建议一并纠偏）

| 文档声明 | 实际 | 建议措辞 |
|---|---|---|
| M1“剪贴板双向 ✅” | macOS RFB 不承载剪贴板；客户端也没接 Agent | 改为“仅对支持 `ServerCutText` 的第三方服务端有效；macOS 需 Agent 通道（未实现）” |
| A2b“系统音频 ✅” | 仅 macOS 采集/编码侧；Windows 无解码播放 | 标注“端到端通路缺少客户端一半” |
| “SSH 隧道（密码/密钥）” | 只有密钥，且首次连接必失败 | 标注已知限制或补齐 |
| “全 workspace 测试通过” | Windows 上 workspace 编译失败 | 注明按平台 `-p` 运行 |
| 渲染 D3D11 | OpenGL(glow) | 更新为现状 + 计划 |

---

## 5. 建议的修复顺序（按收益/成本排）

| 顺序 | 动作 | 预计 | 依据 |
|---|---|---|---|
| 1 | `poll_event` → `recv_event`；`close()` 去 join；连接超时 | 0.5 天 | 2.1 / 2.5，纯本地、立刻见效 |
| 2 | 修修饰键（`mac_cmd`）+ 修饰键状态机 + 特殊键菜单 | 1–2 天 | 2.4，输入是“能不能用”的门槛 |
| 3 | vendor `vnc-rs`：zlib(6)、Apple 1104/1105/1109/1110、未知编码不再致命 | 3–5 天 | 2.2，解锁真机出画质量与光标 |
| 4 | SSH：TOFU 指纹确认 + 密码认证 + stderr 透出 | 2 天 | 2.5，决定首次连接成功率 |
| 5 | 新增 `crates/agent-client`：控制面 → 剪贴板 → 音频 | 8–12 天 | 2.3，方案 C 的另一半 |
| 6 | Agent 签名/公证 + Windows Authenticode + `.gitattributes` | 1–2 天 | 3.1 |
| 7 | 真机验收（30 分钟、fps、输入 P95、Apple 光标） | 需你提供环境 | 2.6 |

---

## 6. 复现方式

两个复现都基于仓库自带的 mock RFB 服务器（`crates/rustvnc-client/tests/session_mock.rs` 的 `handshake`/`serve` 骨架），临时测试文件已删除，需要时可以按下面要点重建：

**未知编码致命**：握手后，对首个 `FramebufferUpdateRequest` 回 `[0,0] [1 rect] (0,0,8,8) encoding=1104` + 64 字节 body，设置 `auto_reconnect = false`，观察 `SessionEvent::Error { code: Protocol }` 后立即 `Closed`。

**空闲烧 CPU**：握手后对每个 `FramebufferUpdateRequest` 只回 `[0,0,0,0]`（空更新），空闲 6 秒，用 `Get-Process <pid>.TotalProcessorTime` 采样，得到 ≈100% 单核。

---

## 7. 一句话总结

> 这个项目的**设计与文档已经跑到 25 周路线图的 M1b**，但**代码实际停在“两个各自能跑的半边”**：macOS Agent 是完整的（音频/剪贴板/QUIC），Windows 客户端是完整的（RFB/UI/配置/SSH），两者之间那根线**一行都还没接**；而在各自半边里，又各有一个“不修就没法发布”的硬伤——客户端是**空闲烧满一个核 + 未知编码即断线**，Agent 是**无签名/无公证装不上**。
> 先把 §5 的 1–4 项（一周内可完成）做完，这个项目就能第一次“在真实 macOS 上稳定出画并可用快捷键”；5–6 项决定它能不能变成产品。
