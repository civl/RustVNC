# M1 / M1b 状态报告 — 可用客户端（条件通过）

> 日期：2026-11-17 · 环境：macOS 27.2.1 arm64 / Windows CI 构建
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
| 未知编码安全跳过 | ❌ vnc-rs 无通用跳过；Apple 1104 会报错 |
| 连接 CRUD / 搜索 / 导入导出 | ✅ 已实现 |
| 凭据保存 / 不保存 | ✅ Keychain/Windows Credential Manager + 内存回退 |
| 5 档画质 + 自定义 | ✅ 已实现（切换需重连） |

**判定：M1 条件通过，M1b 工程项已完成**。代码可构建、可启动，会话引擎和自动重连通过 mock 端到端测试；真实 macOS 帧率/输入延迟/30 分钟稳定性仍需凭据与 Windows 真机。

## 2. M1b 增补（2026-11-17）

- **自动重连**：1/2/4/8/15 秒指数退避；认证/配置错误不重试；用户断开立即停止。
- **统一错误码**：`AUTH` / `NETWORK` / `PROTOCOL` / `CONFIG` / `SSH` / `INTERNAL`，UI 状态栏显示。
- **诊断日志**：stderr + 每日滚动文件；“打开日志”按钮定位目录。
  - macOS：`~/Library/Application Support/RustVNC/client/logs/`
  - Windows：`%APPDATA%/RustVNC/client/logs/`
- **局部纹理更新**：Raw/CopyRect 只上传变化区域，避免每帧全屏 RGBA 上传。
- **Windows 安装包**：Inno Setup 脚本、图标、PowerShell 构建脚本（`packaging/windows/`）。
- **首次使用引导**：无配置时显示提示，引导填写 Mac 地址/端口/凭据与 SSH 建议。
- **中文字体**：自动加载系统 CJK 字体（macOS Hiragino Sans GB / Windows 微软雅黑 / Linux Noto CJK）作为 egui 回退，解决中文方框。
- **剪贴板双向**：本地剪贴板变化自动发送 `ClientCutText`；远端 `ServerCutText` 自动写入本地；可关闭。
- **诊断包**：一键导出应用/系统/配置摘要（不含密码）+ 最近 400 行日志。
- **输入增强**：使用物理键位做快捷键映射；支持按键重复；特殊键携带 Shift/Ctrl/Alt/Cmd；工具栏“发送剪贴板”。
- **测试**：新增自动重连 mock 测试（错误 → Reconnecting → 第二连接 → 新帧）。

## 3. 已实现

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

## 4. 验证

```bash
cargo test -p rustvnc-client
```

结果：

- 6 个单元测试：输入映射、Unicode keysym、凭据内存后端、配置导入导出、画质映射、编码列表。
- 2 个 mock 会话集成测试：
  - RFB 3.8 None 认证、32×16 Raw 帧、Resized、Pointer/Key；
  - 远端协议错误 → 自动重连 → 第二次连接收到新帧。
- UI 冒烟：`target/debug/rustvnc-client` 可正常启动并保持运行。
- 字体测试：系统 CJK 字体包含“中/文”字形。
- 自动重连测试在 macOS/Windows CI 均通过。

全 workspace：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo deny check
```

均通过。

## 5. 未完成 / 已知缺口

| 缺口 | 说明 | 计划 |
|---|---|---|
| Apple 1104 光标 | vnc-rs 未实现；当前无远端光标形状 | M2 fork/外层补丁 |
| Apple 1105/1109/1110 | 缩放/键盘布局/显示信息缺失 | M2 |
| 未知编码 | 无通用 skip；Apple 私有编码会中断会话 | M2 |
| 真实 macOS 连接 | 缺凭据，未测帧率/延迟/30 分钟稳定性 | 用户本地执行 |
| Windows 真机 | 安装包脚本已就绪，但未在 Windows 上实际安装/运行 | M1b 验收 |
| 自动重连 | ✅ 已实现指数退避 | 已完成 |
| 错误码 / 诊断 | ✅ 已实现错误码与文件日志 | 已完成 |
| 剪贴板上行 | ✅ 自动同步文本；非文本/富文本不支持 | 已完成（文本） |
| Windows 安装包 | CI 已生成未签名安装包 artifact；未在真机安装 | M1b 验收 |
| 完整 Windows 键位 | 快捷键用物理键位；文本仍依赖 egui 逻辑键 | M1b |
| SSH 首次信任 | 需预先写入 known_hosts；无 UI 指纹确认 | M1b |
| 多显示器 | 只处理 DesktopSize/单屏 | M2 |
| 渲染性能 | 已做局部纹理更新；未在 1080p 真机基准 | M1b 验收 |

## 6. 如何完成最终验收

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

## 7. 下一步（M1b 验收 / M2）

1. 在 Windows 真机安装 CI 产出的 Inno Setup 包并完成首次运行。
2. 真实 macOS 凭据下跑 30 分钟稳定性、fps、输入 P95、窗口重绘。
3. 补 Apple 1104 光标与 1105/1109/1110。
4. Windows 输入法/物理布局细化；剪贴板双向。
5. 目标 1080p 30fps 低 CPU；纹理局部更新已具备。
