# RustVNC

RustVNC 是一个用 Rust 构建、面向 Windows 用户的 macOS VNC 客户端项目。

## 文档

- [总体设计（需求 / 架构 / 开发路径）](design.md)
- [macOS Agent 设计（B1/B2、音频、视频、权限、配对）](agent-design.md)
- [A0 骨架说明（菜单栏 / SMAppService / 单实例）](README-A0.md)
- [A1 控制面说明（QUIC/TLS / 配对 / Ping-Pong）](README-A1.md)
- [A2 菜单栏服务与剪贴板说明](README-A2.md)

## 当前进度

- **A0 已完成**：macOS Agent 最小骨架
  - `LSUIElement` 菜单栏应用
  - `SMAppService.agent` 注册 LaunchAgent
  - launchd 崩溃自动拉起、正常退出不复活
  - `flock` 单实例
  - 文件日志与基础诊断
- **A1 已完成**：Agent 控制面
  - QUIC + TLS 1.3（ALPN `rustvnc-agent/1`）
  - 一次性配对码、自签 CA、客户端证书（mTLS）
  - `Hello / ServerHello`、`Ping / Pong`
  - 客户端证书白名单（`clients.txt`）
  - CSR 配对（客户端私钥不出本机）
  - CA/服务器/客户端私钥存 Keychain（可 `--no-keychain` 回退文件）
  - 按 IP 限流（配对与连接）
  - 全链路集成测试（配对 / Ping / 拒绝 / 限流）
  - `rvncctl` 命令行客户端（`pair` / `ping`）
- **A2 已完成**：菜单栏服务 + 剪贴板
  - 菜单栏启动时自动启动 QUIC 控制服务
  - 显示服务器地址、配对码、客户端状态；一键复制配对码
  - 停止/启动服务器；重新启动生成新配对码
  - 双向 UTF-8 文本剪贴板（1 MiB，仅同步连接后的变化）
- **下一步（A3）**：Core Audio Tap + Opus 音频

## 目录

```text
src/                # A0 Agent 源码
resources/          # Info.plist 与 LaunchAgent plist
scripts/            # 构建、安装、重置服务
design.md           # 总体设计
agent-design.md     # Agent 详细设计
README-A0.md        # A0 使用说明
README-A1.md        # A1 控制面说明
README-A2.md        # A2 菜单栏与剪贴板说明
```

## 构建 Agent

```bash
cargo build
./scripts/build-app.sh debug
open "dist/RustVNC Agent.app"
```

详见 [README-A0.md](README-A0.md)。

## 许可

MIT，见 [LICENSE](LICENSE)。
