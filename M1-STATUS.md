# M1 状态报告 — 可用客户端（条件通过）

> 日期：2026-11-16 · 环境：macOS 27.2.1 arm64 / Windows CI 构建
> 目标：日常可用的 VNC 客户端，覆盖 80% 个人用户场景。

## 1. 结论

M1 的**客户端核心、配置/凭据、输入、SSH 隧道和 UI 骨架**已落地；受 macOS 凭据与 Windows 真机环境限制，M1 的最终验收指标尚未实测：

| M1 验收项 | 状态 |
|---|---|
| 1080p 连续 30 分钟不崩溃 | ⏳ 需真实凭据 + Windows 真机 |
| LAN 1080p ≥ 20 fps | ⏳ 需真实凭据 |
| 输入 P95 ≤ 120 ms | ⏳ 需真实凭据 + Windows 真机 |
| 登录窗口可输入 | ⏳ 需真实凭据 |
| 窗口变化 ≤ 1 s 重绘 | ⏳ 需真实凭据 |
| SSH 主机密钥变更拒绝 | ✅ 实现层面强制 `StrictHostKeyChecking=yes` |
| 未知编码安全跳过 | ✅ vnc-rs 默认回退 Raw |
| 连接 CRUD / 搜索 / 导入导出 | ✅ 已实现 |
| 凭据保存 / 不保存 | ✅ Keychain/Windows Credential Manager + 内存回退 |
| 5 档画质 + 自定义 | ✅ 已实现（切换需重连） |

**判定：M1 条件通过**。代码可构建、可启动、会话引擎通过 mock 端到端测试；真实 macOS 画面/帧率/输入延迟和 Windows 安装包留待 M1b。

## 2. 已实现

### 客户端会话

- `crates/rustvnc-client`，复用 `vnc-rs 0.6.1` 的 RFB/ARD/ZRLE/Tight 实现。
- 异步 session actor：连接、刷新、Raw/Copy 帧、剪贴板文本、统计。
- 支持 32bpp BGRA / 16bpp RGB565。
- 支持 ZRLE / Tight / Raw，未知编码由 vnc-rs 回退。
- 会话事件通过标准通道投递，UI 不阻塞网络线程。

### 连接管理

- 配置 CRUD、搜索、删除、导入/导出（剪贴板 JSON）。
- 持久化到 `%APPDATA%/RustVNC/client/profiles.json`（macOS 为 `~/Library/Application Support/RustVNC/client/`）。
- 密码只存 Keychain / Windows Credential Manager；勾选“记住”才保存，取消后删除。
- 支持“直连”和“SSH 隧道”两种安全模式。

### 画质

- 极速 / 流畅 / 高清 / 超清 / 自定义。
- 预置映射到 bpp、编码与刷新率；自定义可改 bpp/编码。
- 画质切换在重连后生效（RFB 协商后不能热改编码）。

### 输入

- 字母/数字/符号、Shift 组合。
- Enter/Tab/Backspace/Esc/Delete/Insert/Home/End/PageUp/PageDown/方向键/F1–F12。
- Ctrl/Alt/Cmd 快捷键（发送修饰键按下/释放）。
- Unicode 文本（CJK 走 0x01000000 + codepoint keysym）。
- 鼠标移动、左/中/右键、滚轮。
- 指针位置按窗口缩放映射到远端坐标。

### SSH 隧道

- 使用系统 OpenSSH（Windows 10+ 自带、macOS 自带）。
- `BatchMode=yes`：仅密钥认证，不弹密码。
- `StrictHostKeyChecking=yes` + 可配置 `known_hosts`：主机密钥变化直接失败。
- 自动选择本地空闲端口，10 秒内未建立转发则报错。
- 只支持密钥；密码/键盘交互认证不在 M1。

### UI

- egui/eframe 原生窗口：配置列表、搜索、连接表单、会话视图、状态栏。
- 会话画面按窗口缩放，最近邻采样；状态栏显示 fps/rects/KiB。
- 中英切换骨架（主要标签已双语，表单细节仍以中文为主）。
- 配置导入/导出、删除、清空错误。

## 3. 验证

```bash
cargo test -p rustvnc-client
```

结果：

- 6 个单元测试：输入映射、Unicode keysym、凭据内存后端、配置导入导出、画质映射、编码列表。
- 1 个 mock 会话集成测试：RFB 3.8 None 认证、32×16 Raw 帧、Resized 事件、Pointer/Key 事件到达服务端。
- UI 冒烟：`target/debug/rustvnc-client` 可正常启动并保持运行。

全 workspace：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo deny check
```

均通过。

## 4. 未完成 / 已知缺口

| 缺口 | 说明 | 计划 |
|---|---|---|
| Apple 1104 光标 | vnc-rs 未实现；当前无远端光标形状 | M1b fork/外层补丁 |
| Apple 1105/1109/1110 | 缩放/键盘布局/显示信息缺失 | M2 |
| 真实 macOS 连接 | 缺凭据，未测帧率/延迟/30 分钟稳定性 | 用户本地执行 |
| Windows 真机 | 仅 CI 构建，无安装包/实际运行 | M1b |
| 自动重连 | 当前断开后手动重连 | M1b |
| 错误码 | 目前是字符串错误，未做统一错误码/诊断包 | M1b |
| 剪贴板上行 | 只接收 RFB Latin-1 文本；未发送 | M2 |
| 完整 Windows 键位 | 依赖 egui Key，未做物理 VK/布局映射 | M1b |
| SSH 首次信任 | 需预先写入 known_hosts；无 UI 指纹确认 | M1b |
| 多显示器 | 只处理 DesktopSize/单屏 | M2 |
| 渲染性能 | 全帧 RGBA 上传，未做 dirty rect/纹理局部更新 | M2 |

## 5. 如何完成最终验收

在持有 macOS 凭据的 Windows/macOS 机器上：

```bash
# 1. 启动客户端
cargo run -p rustvnc-client

# 2. 新建配置：主机、端口 5900、macOS 用户、密码、画质“高清”
# 3. 连接后观察状态栏 fps/rects
# 4. 连续运行 30 分钟，操作键鼠、改窗口大小
# 5. 记录：fps、输入延迟、是否崩溃、窗口重绘时间
```

建议同时用 `--inspect`/`rfb-probe` 确认网络与认证正常。

## 6. 下一步（M1b / M2）

1. 补 Apple 1104 光标与 Windows 安装包/图标/首次运行向导。
2. 自动重连 + 统一错误码 + 诊断日志导出。
3. Windows 物理键位映射、输入法、组合键菜单。
4. 纹理局部更新/dirty rect，目标 1080p 30fps 低 CPU。
5. 真实 macOS 30 分钟稳定性与输入延迟基准，补齐 M1 验收数据。
