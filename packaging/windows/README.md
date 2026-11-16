# RustVNC Windows 安装包（M1b）

使用 **Inno Setup 6** 生成 per-user 安装包（无需管理员权限）。

## 构建

```powershell
winget install JRSoftware.InnoSetup
powershell -ExecutionPolicy Bypass -File packaging/windows/build.ps1
```

输出：

```text
packaging/windows/output/RustVNC-Setup-0.1.0.exe
```

安装目录：`%LOCALAPPDATA%\Programs\RustVNC`。

## 说明

- 当前未做代码签名；Windows SmartScreen 可能提示“未知发布者”。
- 安装包只包含客户端；macOS Agent 需单独安装 `.app`。
- 需 Windows 10 1809+ / x64；客户端使用 OpenGL（eframe/glow）。
- 首次启动会显示“首次使用”提示，引导填写 Mac 地址、端口和凭据。
- 建议在客户端“安全”中选择 SSH 隧道；需要系统 OpenSSH（Windows 10+ 自带）。
