# M2 执行计划 — Windows 原生体验

> 目标：从“能跑”到“好用”，达到可发布质量。
> 基线：M1/M1b 已通过（真机 5K/1080p 出画、ZRLE/zlib/16bpp、Agent 通道、SSH、安装包 CI）。

## 出口标准（design.md）

- 4K 显示器本地呈现 60 Hz 不掉帧；200% DPI 文字清晰。
- 安装 → 升级 → 回滚全流程通过；SmartScreen 不拦截。
- 弱网（10% 丢包 / 200 ms RTT）10 秒内自动降档且不振荡。

## 工作流

| 编号 | 工作流 | 当前状态 | 本轮 |
|---|---|---|---|
| M2a | 渲染 / DPI / 多显示器 / 比例切换 | 部分完成：D3D12/Vulkan/GL/glow 回退；全屏；Fit/1:1/50/100/200 + 滚动；多显示器仍为合并 framebuffer | 本轮完成比例/DPI |
| M2b | 自适应控制器：快/慢环、滞回、手动锁定、HUD | ✅ 本轮已实现：延迟 EMA、双阈值、HUD、开关 | 完成 |
| M2c | 多标签 / 托盘 / 通知 / 开机自启 | ✅ 多标签、托盘、开机自启已实现；通知待做 | 完成（通知除外） |
| M2d | MSI/Authenticode/自动更新/回滚 | 自动更新/校验/回滚 + Inno + WiX MSI CI 产物已验证；真实证书签名待补 | 基本完成 |
| M2e | 性能/内存/CPU/8 小时 soak | 未开始 | 后续 |

## M2b：自适应控制器（本轮）

现状：

- `FramePacer` 只在连续 3 个无像素帧后把请求间隔降到 250 ms。
- 没有测量“请求 → 完整帧”延迟，也没有弱网降档/恢复滞回。

本轮实现：

1. 在 `FramebufferUpdateRequest` 到 `UpdateComplete` 之间测帧延迟。
2. 指数移动平均（EMA）+ 双阈值滞回：
   - 连续高延迟 → 请求间隔增大（最多 500 ms）。
   - 连续低延迟 → 逐步恢复，但不低于当前画质档的基准帧率。
   - 中间区间不动作，避免抖动。
3. `adaptive` 配置开关；关闭后只保留静止降频。
4. HUD 显示延迟、实际请求间隔、是否降档，以及“静止降频”原因。
5. 单元测试覆盖：高延迟降档、低延迟恢复、滞回不振荡、关闭自适应。

手动锁定：通过关闭 `adaptive` 实现（固定为画质档帧率 + 静止降频）。

实现位置：`crates/rustvnc-client/src/session.rs`（`FramePacer`、`SessionEvent::Stats`）；
配置与 UI：`config.rs` 的 `adaptive`、`main.rs` 的“自适应帧率”开关。

## M2c：多标签 / 托盘 / 开机自启

- 顶部标签栏：每个标签保留独立 RFB 会话；后台标签暂停请求并丢弃帧，切回时发 `FullRefresh`，不会在后台堆积帧内存。
- 每个标签独立 profile、Agent 通道、剪贴板基线、键鼠状态；后台音频停止，切回重连 Agent。
- 托盘：显示主窗口 / 退出；关闭窗口默认隐藏到托盘，托盘退出才真正结束。
- 开机自启：`auto-launch` per-user；登录时以 `--minimized` 启动并收进托盘。
- 未做：通知、标签跨重启恢复、每标签独立托盘/缩略图。

## M2a：视图比例与 HiDPI

- 会话工具栏新增“视图”：适应窗口、1:1 像素、50%、100%、200%。
- 1:1 使用 `pixels_per_point`：1 个远端像素对应 1 个物理屏幕像素。
- 超出视口时自动进入双向滚动区；小于视口时居中。
- 选择写入 profile，重连后保留。
- 多显示器仍是 macOS 合成的单个 framebuffer；未做按显示器裁切/选择。

## M2d：安装/签名/更新

已实现：

- 更新 manifest：`version/url/sha256/notes`，按版本比较。
- 下载后强制 SHA-256；Windows 再用 `Get-AuthenticodeSignature` 要求 `Valid`。
- 更新前把当前 exe 备份到 `updates/rollback/`；失败或手动“回滚”可启动旧版本。
- 静默安装：Inno `/SILENT /NORESTART /CLOSEAPPLICATIONS`，MSI `msiexec /qn`。
- CI 产物：`RustVNC-Setup-0.1.0.exe` 与 `RustVNC.msi`（WiX 3.14，per-user）。
- UI：“检查更新 / 回滚”，更新窗口显示版本与说明。
- `RUSTVNC_UPDATE_URL` 覆盖 manifest；开发可 `RUSTVNC_ALLOW_UNSIGNED_UPDATE=1` 跳过 Authenticode（默认不允许）。

未完成：

- 真实证书的 Authenticode、SmartScreen/EV 验证。
- 更新服务器/Release 资产与自动检查策略。

## 后续顺序

1. M2d MSI/签名/更新与回滚；补 Authenticode。
2. M2e 8 小时 soak、CPU/内存预算；补 4K60 本地呈现基准。
3. M2a 收尾：多显示器裁切/选择、4K60 本地呈现。
4. M2c 收尾：通知、标签跨重启恢复、托盘快捷键。
