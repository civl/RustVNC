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
| M2a | 渲染 / DPI / 多显示器 / 比例切换 | 部分完成：D3D12/Vulkan/GL/glow 回退；全屏；Fit/1:1/50/100/200 + 滚动；**分辨率自动匹配客户端屏幕 + 帧率 <30 逐级下退（真机验证，见下）**；多显示器仍为合并 framebuffer | ✅ 比例/DPI/自动降分辨率 |
| M2b | 自适应控制器：快/慢环、滞回、手动锁定、HUD | ✅ 本轮已实现：延迟 EMA、双阈值、HUD、开关 | 完成 |
| M2c | 多标签 / 托盘 / 通知 / 开机自启 | ✅ 多标签、托盘、开机自启、工作区跨重启（标签+窗口几何+语言）已实现；通知待做 | 完成（通知除外） |
| M2d | MSI/Authenticode/自动更新/回滚 | 自动更新/校验/回滚 + Inno + WiX MSI + tag 发布工作流；真实证书签名待补 | 基本完成 |
| M2e | 性能/内存/CPU/8 小时 soak | ✅ 工具就绪：`scripts/bench-soak.ps1`（自动连接 + 采样 + 稳态回归 + 阈值）与 `--bench-frames`（本地呈现）、`tests/input_latency.rs`（输入→像素 p50 守门，p95 记录）；已跑 2 分钟冒烟：空闲 CPU 0.88%、内存平稳 | 待真机 8h/4K60 |

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
- 工作区跨重启：`ui.json`（与 `profiles.json` 同目录）记住打开的标签、激活项、窗口几何与
  界面语言；重启只还原标签、**不自动重连**（否则登录时拉起 N 条会话）。配置删除后对应标签
  自动丢弃，损坏的 `ui.json` 只记日志、不阻塞启动。
- 单实例互斥：`Global\RustVNC.Client.<用户 SID>` 命名互斥量——**按用户**而非按会话，
  同一用户在远程桌面里再开一个也会被拦下；创建全局对象被策略拒绝时回落到 `Local\`。
  第二个启动不建窗口，只往 `.Activate` 事件发一次信号就退出；已运行的实例收到后从托盘
  恢复、前置并聚焦。自动化脚本用 `RUSTVNC_ALLOW_MULTIPLE=1` 显式放行（`bench-soak.ps1` 已设置）。
- 未做：通知、每标签独立托盘/缩略图。

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
- Tag 发布：`release.yml` 构建 EXE/MSI、生成 `update.json`、创建 GitHub Release。
- UI：“检查更新 / 回滚”，更新窗口显示版本与说明。
- `RUSTVNC_UPDATE_URL` 覆盖 manifest；开发可 `RUSTVNC_ALLOW_UNSIGNED_UPDATE=1` 跳过 Authenticode（默认不允许）。

未完成：

- 真实证书的 Authenticode、SmartScreen/EV 验证。
- 更新服务器/Release 资产与自动检查策略。

## M2e：soak 与基准（本轮）

已实现：

- `scripts/bench-soak.ps1`：启动（或 `-Attach` 附着）客户端，每 N 秒采样 CPU/内存/句柄/线程，
  写 CSV 与 summary.json；泄漏判定用**稳态窗口回归斜率**（跳过开头 10% 的分配期），
  且要求窗口 ≥ 30 分钟、绝对增长 > 30 MB，避免短窗口噪声外推。
- `--connect <配置名>`：无人值守连接，soak 与基准都靠它。
- `--bench-frames <秒> --bench-out <文件>`：本地呈现统计（`unstable_dt` 分位数），
  跳过 1 秒预热，测完自行退出（关窗会收进托盘，不能用来结束基准）。
- `tests/input_latency.rs`：mock 服务端只在被请求时发送像素，测"输入 → 像素到达"的 P95；
  预算 20 ms，同时守护"输入是否唤醒请求循环"。

首轮发现并修复的两个问题：

1. 空闲窗口仍按 60 Hz 重绘：空闲 CPU 10.98% → 0.88%（无会话时改为 250 ms 节拍）。
2. 空闲降频后首次输入要等下一个轮询周期：输入现在立即请求下一帧（在途则等其完成立刻补发），
   输入→像素 p50 从 47 ms 降到 0.8 ms。


## 真机验证：自动降分辨率（2026-11-19，Mac mini M4 / macOS 27.2.1）

环境：BenQ SW270C 5120×2880（主，UI 2560×1440）+ BetterDisplay 虚拟 16:9 3840×2160，
合成 framebuffer **8960×2880**；客户端为 2560×1440 的 Windows 主机。

实测链路（经 Tailscale，headless Agent + `agent_probe`）：

```text
auto resolution: client screen Some((2560, 1440)) -> 2560x1440
agent channel connected agent=RustVNC Agent 0.1.0 capabilities=6
DISPLAY 2560x1440 applied=true: 2560x1440 (2560x1440 px)
first frame 2560x1440            # 原 8960x2880
```

| 指标 | 8960×2880（原生） | 2560×1440（自动降档后） |
|---|---|---|
| framebuffer 像素量 | 25.8 M | 3.7 M（**1/6.9**；无虚拟屏时相对 8960 为 1/13.8） |
| RFB 吞吐（16bpp ZRLE） | 50–58 rects/s，~412–470 KB/s | 同量级但每帧像素更少 |
| 帧延迟 | **233–600 ms**，自适应顶到 500 ms 上限 | 未再触发降档 |
| 本地呈现 | — | **59.9 fps**，p50 16.67 ms / p95 16.97 ms / p99 17.18 ms（1440 帧） |

恢复行为（三种路径均验证）：

1. 会话正常结束 → `DisplayGuard::drop` 恢复 5120×2880
2. 客户端被强杀 → Agent 监听 `connection.closed()`，恢复
3. Agent 崩溃后重启 → 启动时 `restore_saved()` 恢复残留模式

### 权限边界（重要，纠正此前判断）

- `CGDisplayCopyAllDisplayModes` / `CGConfigureDisplayWithDisplayMode`：**不需要**任何 TCC 授权，
  SSH 会话里同样可用（枚举到 451 个模式，切换成功）。
- `CGGetActiveDisplayList`：**需要“屏幕录制”权限**；无授权时返回 **0 块屏**，
  这使 `MirrorGuard` 遍历空列表、返回成功但什么都不做。
  已用独立 Swift 程序 + 项目二进制在 SSH / `open` / LaunchAgent 四种上下文复现。
- 因此：降分辨率零授权可用；**单屏镜像**必须先给 Agent 授予屏幕录制权限。

### 踩坑

切到非 HiDPI 模式会让 HiDPI 模式从模式列表消失，需重启 BetterDisplay 重新探测。
所以上面第 2、3 条恢复路径是必需项，不是可选优化。
## 后续顺序

1. M2d MSI/签名/更新与回滚；补 Authenticode。
2. M2e：工具已就绪；1440p 本地呈现已实测 59.9 fps（见"真机验证"），待跑 8 小时 soak 与 4K60 基准。
3. M2a 收尾：多显示器裁切/选择；单屏镜像需先授予屏幕录制权限。
4. M2c 收尾：通知、标签跨重启恢复、托盘快捷键。
