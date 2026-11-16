# RustVNC

RustVNC 是一个用 Rust 构建、面向 Windows 用户的 macOS VNC 客户端项目。

## 文档

- [总体设计（需求 / 架构 / 开发路径）](design.md)
- [macOS Agent 设计（B1/B2、音频、视频、权限、配对）](agent-design.md)
- [A0 骨架说明（菜单栏 / SMAppService / 单实例）](README-A0.md)

## 当前进度

- **A0 已完成**：macOS Agent 最小骨架
  - `LSUIElement` 菜单栏应用
  - `SMAppService.agent` 注册 LaunchAgent
  - launchd 崩溃自动拉起、正常退出不复活
  - `flock` 单实例
  - 文件日志与基础诊断
- **下一步（A1）**：QUIC/TLS 传输、配对、控制协议、客户端联调

## 目录

```text
src/                # A0 Agent 源码
resources/          # Info.plist 与 LaunchAgent plist
scripts/            # 构建、安装、重置服务
design.md           # 总体设计
agent-design.md     # Agent 详细设计
README-A0.md        # A0 使用说明
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
