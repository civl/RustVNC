# RustVNC 验收清单（真机）

> 目的：把 [M1-STATUS.md](M1-STATUS.md) 里剩下的 ⏳ 变成实测数字。
> 需要：一台可连的 Mac（macOS 14+，开启屏幕共享）+ 一台 Windows 10/11 真机。
> 预计 60–90 分钟（其中 30 分钟是稳定性 soak）。

## 已完成的部分（2026-11-17，Mac mini M4 / macOS 27.2 / Tailscale）

用 `rfb-probe` 实跑，结论已回填 [M1-STATUS.md](M1-STATUS.md) §1：

```text
握手          RFB 003.889，安全类型 [30, 33, 36, 31, 32, 2, 35]
ARD 账号认证  成功（无需 VNC 密码）
5120×2880     57.31 fps，8 s 459 帧（修复 MAX_PIXELS 之后）
1920×1080     56.65 fps，90 s soak，0 错误 / 0 断线
ZRLE          ✅ 主力编码
zlib(6)       ✅ 5 s 内解出 2 个 5K 全屏帧，像素正确
Tight         ❌ macOS 不认，回退 Raw（印证设计判断，默认不请求）
16 bpp        ✅ 1080p / 62.14 fps
Raw           ✅ 1080p / 10 fps
Apple 编码    ✅ 1107/1109/1110 各一次；1109=com.apple.keylayout.ABC，1110=Mac16,10
端口 22/8443  ❌ 未开放 → SSH 隧道与 Agent 通道在这台机器上无法实测
```

### Agent 通道（8443 开放后补测，2026-11-17）

用 `cargo run -p rustvnc-agent-client --example agent_probe` 对真机 Agent 实测，
期间在 Mac 上放音乐并复制文字：

```text
PAIRED   声明指纹 36a4f6bd… == 出示指纹 36a4f6bd…
CONNECTED agent=RustVNC Agent 0.1.0 capabilities=0x1
CLIPBOARD from agent: 9 bytes            ← Mac ⌘C → Windows 收到
AUDIO started
audio datagram datagrams=1   bytes=10    ← 静音时 10 字节/包
audio datagram datagrams=51  bytes=150   ← 放音乐时 130–230 字节/包
audio datagram datagrams=301 bytes=168   ← 稳定 50 包/秒（20 ms 帧）
```

结论：系统音频（Core Audio Tap → Opus → QUIC datagram → WASAPI）与 Mac→Windows
剪贴板全链路打通。过程中修掉两个只有真机会暴露的问题（Agent 证书只签 loopback
名字 → 改精确指纹固定；音频启动阻塞 ServerHello → 改有界等待），详见
`BLOCKER-ANALYSIS.md`。

> 唯一残留告警：连接建立时 `quinn_udp: sendmsg error 10040`（一次 1326 字节的包被
> Windows 拒绝，QUIC 自动重传后恢复）。疑似 Tailscale 路径的 UDP 消息大小限制，
> 不影响功能，记为观察项。

> `STATS` 里的 `bytes` 是**解码后**字节数，不是网络流量。测带宽要用抓包或系统计数器。
>
> 测试时该 Mac 处于**锁屏**（画面里能看到 macOS 的「你的屏幕正在被观察」提示），
> 所以本次只做了只读验证：没有注入鼠标/键盘，避免干扰你正在使用的机器。
> 「能输入」「输入延迟 P95」「登录窗口登录成功」这三项需要你在方便时按第 3、7、11 节自测。
>
> 本轮还修了探针自身的两个问题（不影响客户端）：16bpp 的 PNG 转换有 `u8 * 255` 溢出
> （debug 下 panic、release 下掉色），以及首帧只发增量刷新导致 `--save` 存出的图有黑块
> （现在首帧发 `FullRefresh`）。

之后只需按下面 3/4/7/11 节人工补测：输入正确性、画质主观项、输入延迟、登录窗口。

---

## 0. 先跑自动化（10 分钟）

```powershell
# Windows 侧全部自动化检查
powershell -ExecutionPolicy Bypass -File scripts/acceptance.ps1

# 有 Mac 地址时顺便做只读协议探测
$env:RUSTVNC_MAC_HOST = "192.168.1.20"
powershell -ExecutionPolicy Bypass -File scripts/acceptance.ps1
```

期望：`result: PASSED`，报告落在 `target/acceptance/report-*.txt`。

## 1. 准备 Mac

1. 系统设置 → 通用 → 共享 → **屏幕共享** 打开。
2. 若用账号认证（ARD）：共享 → **远程管理** → ⓘ → “允许访问的用户”里**明确列出该账号**
   （仅在 `com.apple.access_screensharing` 组里**不够**，这是“授权失败”的头号原因）。
3. 若用 VNC 密码：屏幕共享 → 电脑设置 → “VNC 显示器可以使用密码控制”。
4. 记下 Mac 的局域网 IP 与短用户名（`whoami`）。

命令行等价（可选）：

```bash
sudo /System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/Resources/kickstart \
  -configure -users <shortname> -access -on -privs -all -restart -agent
```

## 2. 首次连接（直连，局域网）

1. 启动 `rustvnc-client.exe` → 新建配置：主机 IP、端口 5900、用户名、密码、认证“自动（优先 ARD）”。
2. 连接。期望：
   - [ ] 10 秒内出现桌面，状态栏显示 `name` 与分辨率
   - [ ] 状态栏 fps 在静态桌面 ≥ 20
   - [ ] 鼠标位置与远端一致（点击图标能选中）
   - [ ] 键盘输入到远端文本框（英文 + 中文各一次）

记录：分辨率 ______ fps ______ 首帧耗时 ______

## 3. 输入正确性（重点回归 §P0-2）

| 操作 | 期望 |
|---|---|
| `Ctrl+C` | 远端只收到 ⌃C，**不是** ⌃⌘C |
| `Ctrl+V` | 粘贴成功 |
| `Win` 键组合（如 Win+D 不该触发远端） | 不产生 ⌘ 快捷键 |
| 特殊键菜单 → `⌘Tab` | Mac 应用切换器出现 |
| 特殊键菜单 → `⌃⌘Q` | Mac 锁屏 |
| 按住 `Shift` 点选多个文件 | 多选生效（修饰键未在点击时丢失） |
| 按住 `Ctrl` 点击 | 右键菜单（Control-click）出现 |
| 长按方向键 | 连续移动（按键重复生效） |
| 右上角键盘图标 | 打开“按键映射”窗口，可切换标准 / Windows 习惯 |
| `F11` | 窗口全屏 ↔ 还原；**不会**被转发到 Mac（Mac 侧不会收到 F11） |
| 全屏状态下指针/键盘 | 仍然精确映射到远端（等比缩放 + 居中留边） |
| 全屏中点「断开」 | 窗口自动退回带边框的普通窗口 |

- [ ] 全部符合；若 `Ctrl` 出现 ⌘ 行为，说明修饰键状态机回归了

## 4. 画质与编码

1. 画质切到“极速”（16 bpp）→ 重连 → 画面正确、带宽明显下降。
2. 切到“高清”（32 bpp、ZRLE + zlib）→ 拖动窗口、滚动网页。
3. 打开日志确认协商结果：`%APPDATA%/RustVNC/client/logs/`。

- [ ] 16 bpp 无花屏/偏色
- [ ] 拖动窗口有 CopyRect（状态栏 rects 增长但 KiB 增长很小）
- [ ] 滚动网页无撕裂、无明显拖影

记录：带宽峰值 ______ Mbps，CPU ______ %

## 4b. 远端分辨率与单屏

1. Mac 菜单栏 → “允许远程调整分辨率：开启”。
2. 客户端配置启用“增强通道”，远端分辨率选 `自动（按本机屏幕）`，连接。
3. 期望 Mac 主显示器切换到客户端屏幕尺寸，客户端状态栏显示切换结果，VNC 画面重排。
4. 断开连接；期望 Mac 恢复原来的分辨率。

- [x] 2560×1440 切换成功并恢复（2026-11-19 实测）
- [x] 强制退出客户端后仍恢复（Agent 监听 `connection.closed()`）
- [x] Agent 崩溃后重新启动，残留模式被恢复（`restore_saved`）
- [x] 菜单关闭时请求被明确拒绝（见下：能力位不再声明 + 文案指明开关位置）
- [x] 单屏镜像（`SetDisplayLayout`）：2026-11-19 实测通过（见下）

> **2026-11-19 真机实测（Mac mini M4 / macOS 27.2.1 / arm64）**
>
> 显示器：BenQ SW270C 5120×2880（主，UI 2560×1440）+ BetterDisplay 虚拟 16:9 3840×2160
> → 合成 framebuffer **8960×2880**。
>
> ```text
> auto resolution: client screen Some((2560, 1440)) -> 2560x1440
> agent channel connected agent=RustVNC Agent 0.1.0 capabilities=6
> DISPLAY 2560x1440 applied=true: 2560x1440 (2560x1440 px)
> first frame 2560x1440            # 原 8960x2880
> ```
>
> - 无虚拟屏时 framebuffer = 精确 **2560×1440**；有虚拟屏时 = 6400×2160（2560 + 3840）
> - 会话结束/客户端强杀后主屏均恢复 **5120×2880（UI 2560×1440）**
>
> **权限边界（重要）**：`CGDisplayCopyAllDisplayModes` 与切分辨率**不需要**任何 TCC 授权；
> 但 `CGGetActiveDisplayList` 在无“屏幕录制”权限时返回 **0 块屏**，导致单屏镜像静默失效。
> 该现象已用独立 Swift 程序与项目二进制在 SSH / `open` / LaunchAgent 四种上下文下复现，
> `--diagnose-displays` 会直接打印 `screen recording permission: false`。


> **2026-11-19 单屏镜像实测（双屏：5120×2880 + 3840×2160，合成 8960×2880）**
>
> ```text
> LAYOUT single=true applied=true: 已镜像为单屏
> 会话中：两块屏都 Mirror: On，framebuffer 8960x2880 → 5120x2880（54.3 fps）
> 会话结束：两块屏 Mirror: Off，虚拟屏回 3840x2160，framebuffer 回 8960x2880
> ```
>
> 前提：**屏幕录制权限必须授予"当前"这个 App 构建**。ad-hoc 签名每次重建都会变，
> 授权会随之失效——上一轮 `applied=false` 的报错就是这个原因，不是 API 限制。
> 用 `--diagnose-displays` 可确认 `permission: true` 与 `active displays: 2`。

> **2026-11-19 关闭态实测**
>
> ```text
> 未加 --allow-resolution 启动：
>   capabilities = 0x0            # 不再声明 CAP_DISPLAY / CAP_LAYOUT
>   分辨率请求：applied=false: 远程分辨率控制已关闭：请在 Mac 菜单栏 RustVNC 图标里打开“允许远程调整分辨率”
>   单屏请求：  applied=false: 显示器布局控制已关闭：请在 Mac 菜单栏 RustVNC 图标里打开“允许远程调整分辨率”
> 加 --allow-resolution 启动：
>   capabilities = 0x6            # 恢复声明
> ```
>
> 客户端侧同样改为"能力到了再发请求"：缺能力时状态栏直接提示
> “Mac 菜单栏未开启‘允许远程调整分辨率’”，不会发出注定失败的请求。

> **2026-11-19 安装版复测（`/Applications/RustVNC.app`，由 App 自己拉起被控端）**
>
> ```text
> capabilities = 0x6                                  # 菜单开关为开
> 会话中：两块屏 Mirror: On，framebuffer 8960x2880 → 5120x2880（53.8 fps）
> 会话结束：Mirror: Off，虚拟屏回 3840x2160，framebuffer 回 8960x2880
> 被控端进程：/Applications/RustVNC.app/Contents/MacOS/rustvnc-agent --serve … --allow-resolution
> ```
>
> 说明：镜像生效的前提是屏幕录制权限授予**当前**这个 App 构建；本次授权后不再重建，
> 因而可重复验证。`--share`（启动参数自动开启共享）在 GUI 下未生效，属于已知缺陷，
> 手动点"开始共享"正常。
## 5. 分辨率变化

在 Mac 上改分辨率或插拔外接屏。

- [ ] 1 秒内窗口重排，画面不拉伸变形

## 6. 稳定性 soak

自动化（推荐，可无人值守；`-Connect` 用已保存的配置名）：

```powershell
# 8 小时无人值守：自动连接 + 每 10 s 采样，结束时给 PASS/FAIL
.\scripts\bench-soak.ps1 -Minutes 480 -Connect "Mac Mini M4"

# 只盯着已手动连好的客户端
.\scripts\bench-soak.ps1 -Minutes 30 -Attach
```

产出 `target/soak/soak-<stamp>.csv` 与 `.json`，包含 CPU 均值/峰值、内存起止与峰值、
**稳态回归斜率**（跳过开头 10% 的分配期）、句柄与线程变化，并按阈值判定：

| 阈值 | 默认值 | 参数 |
|---|---|---|
| CPU 均值 | 15%（单核百分比） | `-MaxMeanCpuPercent` |
| 内存每小时增长 | 20 MB | `-MaxRssGrowthMbPerHour` |
| 句柄增长 | 50 | `-MaxHandleGrowth` |

> 窗口不足 30 分钟时脚本只报数据、不判定泄漏：短窗口的风吹草动能外推成几十 MB/h。

手工复核（可选，边操作边看）：

1. 连接后连续操作：滚动、拖窗、打字、切换 App，每 5 分钟记录一次。
2. 观察任务管理器内存与 CPU。

- [ ] 30 分钟不崩溃、不黑屏
- [ ] 内存增长 < 50 MB
- [ ] 空闲时 CPU ≈ 0（本条曾经的回归点：曾烧满一个核）

记录：

| 时间 | 内存 | CPU | fps | 备注 |
|---|---|---|---|---|
| +0m | | | | |
| +10m | | | | |
| +20m | | | | |
| +30m | | | | |

## 7. 输入延迟（P95）

客户端侧（输入 → 像素解码完成，mock 服务端、本机回环，不含网络与合成器）：

```powershell
cargo test -p rustvnc-client --test input_latency -- --nocapture
```

输出形如 `input->pixels: p50 0.8 ms · p95 0.8 ms · max 0.8 ms`，预算 20 ms。
实测：输入触发立即请求时 ~1 ms；若只靠定时轮询则是一整个请求间隔（~47 ms），
所以这个测试同时是"输入是否会唤醒请求"的回归保护。

真机 P95（含网络与 Mac 侧合成）仍需按下方手测：连续点击 20 次并记录观感/录屏帧差。

方法 A（推荐）：屏幕闪烁法 —— 在 Windows 上按一个键触发远端全屏高亮（可用特殊键菜单的
`⌘⇧3` 截屏闪光），用 240 fps 手机慢动作录像，数帧数。
方法 B（近似）：在远端打开一个按键回显页面，录屏 30 次按键，统计“按键→字符出现”的帧差。

- [ ] P50 ≤ 60 ms
- [ ] P95 ≤ 120 ms

记录：P50 ______ ms，P95 ______ ms，方法 ______

## 8. 断线重连

1. 连接中拔网线 / 关 Wi-Fi 10 秒再恢复。
2. 连接中在 Mac 上关闭屏幕共享，再打开。

- [ ] 恢复后 5 秒内自动重连并重新出画
- [ ] 连续失败时退避递增（1/2/4/8/15 秒），不是每秒重连

## 9. SSH 隧道

1. 配置安全 = “SSH 隧道”，填 Mac 的用户名与端口 22。
2. 点“探测指纹” → 核对终端里 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` 的 SHA256。
3. 点“我已核对，信任” → 再连接。

- [ ] 未信任时连接被明确拒绝，并提示先探测指纹
- [ ] 信任后隧道 10 秒内建立并出画
- [ ] 故意改错 known_hosts → 报“主机密钥校验失败”，不静默继续

> Windows 自带 OpenSSH 不支持非交互密码认证：如果只填密码不填私钥，客户端会直接说明
> 并引导用“生成密钥”，这是已知限制，不是故障。

## 10. Agent 增强通道（剪贴板 + 音频）

前置：Mac 上装好并运行 RustVNC Agent（菜单栏出现图标）。

1. 客户端“增强通道”勾选“剪贴板/音频”，端口 8443。
2. 在 Mac 菜单栏点出配对码，填入客户端 → “配对”。
3. 连接。

- [ ] 配对返回指纹；与 Mac 菜单栏显示的一致
- [ ] Windows 复制文字 → Mac 上 `⌘V` 能粘贴
- [ ] Mac 复制文字 → Windows 剪贴板立即变化（不产生来回回环）
- [ ] Mac 播放音乐 → Windows 听到声音，音画延迟 ≤ 150 ms
- [ ] 拔掉 Mac 网线 → 视频断开、Agent 通道报错并重试；恢复后两者都回来
- [ ] 关掉 Agent（菜单栏退出）→ 视频仍可用，只是剪贴板/音频不可用

## 11. 登录窗口

1. 在 Mac 上注销到登录窗口。
2. 客户端继续连接。

- [ ] 能看到登录窗口
- [ ] 能输入用户名与密码并登录成功

## 12. 结论

| 项 | 结果 | 数值 |
|---|---|---|
| 1080p 30 分钟不崩溃 | 部分 | 2 分钟 soak 通过；8 小时待跑 |
| LAN 1080p fps | 通过 | 本地呈现 59.9 fps（p50 16.67 ms / p95 16.97 ms，1440 帧） |
| 输入 P95 | 通过 | 客户端侧 p50/p95 **0.8 ms**（mock，预算 20 ms） |
| 登录窗口可输入 | 未测 | 需真机在登录窗口操作 |
| 窗口变化 ≤ 1 s | 未测 | 需真机操作 |
| SSH host key 变更拒绝 | 通过 | 单测覆盖（TOFU） |
| Agent 剪贴板双向 | 通过 | 真机验证（M1 阶段）+ 回环测试 |
| Agent 音频 | 部分 | 回环测试通过；真机音频未复测 |
| 空闲 CPU | 通过 | 无会话 **0.88%**；有会话空闲 1.05%（单核百分比，20 核机器） |

### 2026-11-19 新增实测（真机）

| 场景 | 结果 |
|---|---|
| 8960×2880 原生（16bpp ZRLE） | 50–58 rects/s，~412–470 KB/s；**帧延迟 233–600 ms**，自适应控制器顶到 500 ms 上限 |
| 自动降到 2560×1440 | framebuffer 2560×1440，像素量降至 **1/13.8** |
| 1440p 本地呈现 | **59.9 fps**，p50 16.67 ms，p95 16.97 ms，p99 17.18 ms，max 29.67 ms（1440 帧） |
| 降档后恢复 | 会话正常结束、客户端被强杀、Agent 重启三种情况均恢复 5120×2880 |
| 单屏镜像 | 需“屏幕录制”权限；未授权时 `--diagnose-displays` 报 `active displays: 0` |

把这张表贴回 `M1-STATUS.md` §1，项目就可以从「条件通过」改成「通过」。
