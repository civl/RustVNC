# RustVNC Agent — A0 骨架

这是 `agent-design.md` 中 A0 阶段的最小可运行骨架：

- 一个 `LSUIElement` 的 `.app`（无 Dock 图标，菜单栏常驻）
- 菜单栏图标与菜单（状态、启用/停用登录项、打开登录项设置、日志、退出）
- `SMAppService.agent(plistName:)` 注册 `Contents/Library/LaunchAgents/com.rustvnc.agent.plist`
- launchd `KeepAlive = { SuccessfulExit: false }`：崩溃自动拉起，主动退出不复活
- `flock` 单实例：第二个实例直接 exit 0，不会造成 launchd 重启风暴
- 文件日志：`~/Library/Logs/RustVNC/agent.log.YYYY-MM-DD`

尚未实现（后续 A1/A2）：QUIC/TLS、配对、音频、剪贴板、视频、输入。

## 构建

要求：macOS 14+、Xcode Command Line Tools、Rust stable。

```bash
cargo build
./scripts/build-app.sh debug
```

产物：`dist/RustVNC Agent.app`

默认使用 **ad-hoc 签名**。为了让 `SMAppService` 在重建后仍能稳定匹配 launchd 的代码约束（LWCR），建议用本机 Apple Development 证书签名：

```bash
CODESIGN_IDENTITY="Apple Development: Your Name (TEAMID)" ./scripts/build-app.sh debug
```

查看可用证书：

```bash
security find-identity -v -p codesigning
```

## 运行

```bash
open "dist/RustVNC Agent.app"
```

菜单栏会出现一个圆环图标。日志：

```bash
tail -f ~/Library/Logs/RustVNC/agent.log.*
```

## 安装到 /Applications（可选）

开发时从 `dist/` 运行即可。若要作为真正的登录项长期使用，建议安装到 `/Applications`：

```bash
./scripts/install-app.sh
# 如果当前用户对 /Applications 没有写权限：
sudo ./scripts/install-app.sh
```

脚本会先注销旧服务，再复制 bundle 到 `/Applications`，从新位置重新注册并打开。

## 注册 / 查看 / 注销登录项

```bash
BIN="dist/RustVNC Agent.app/Contents/MacOS/rustvnc-agent"

"$BIN" --status       # 未注册 / 已启用 / 需在系统设置中批准
"$BIN" --register     # 注册 LaunchAgent，launchd 会立即启动它
"$BIN" --unregister   # 注销（会终止正在运行的 agent）
```

验证 launchd 状态：

```bash
launchctl print "gui/$(id -u)/com.rustvnc.agent"
```

如果系统弹出了“已添加后台项目”的通知，或状态是“需在系统设置中批准”，打开：

```
系统设置 → 通用 → 登录项与扩展
```

菜单里的“打开登录项设置…”也会跳转过去。

## 重建后服务起不来？

如果在已注册状态下重新 `build-app.sh`（替换了 bundle 的代码签名），launchd 可能因为旧的 LWCR 缓存而报：

```
OS_REASON_CODESIGNING | Launch Constraint Violation
```

执行一次干净的 reset：

```bash
./scripts/reset-service.sh
```

它会 `launchctl bootout` → `--unregister` → `--register`，然后重新拉起。

## 单实例验证

先运行 app，再运行一次二进制：

```bash
"dist/RustVNC Agent.app/Contents/MacOS/rustvnc-agent"
```

第二个实例会立即 exit 0，并在日志里写：

```
WARN ... another instance is already running ...; exiting
```

## 崩溃 / 退出行为

- `kill -9 <pid>`：launchd 在 `ThrottleInterval`（10s）后自动拉起。
- `kill -TERM <pid>`：Agent 捕获 SIGTERM，干净退出（exit 0），launchd 不会立即复活；下次登录或 `launchctl kickstart` 再启动。
- 菜单“退出 RustVNC Agent”：同上。

## 目录

```
Cargo.toml
src/main.rs              # 入口、CLI、事件循环、菜单
src/smappservice.rs      # SMAppService 注册/状态/注销
src/single_instance.rs   # flock 单实例
src/icon.rs              # 22x22 模板图标（圆环）
resources/Info.plist
resources/com.rustvnc.agent.plist
scripts/build-app.sh
scripts/reset-service.sh
scripts/install-app.sh
```
