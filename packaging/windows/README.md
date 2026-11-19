# RustVNC Windows 安装包

使用 **Inno Setup 6** 生成 per-user 安装包（无需管理员权限）。

## 构建

```powershell
winget install JRSoftware.InnoSetup
choco install wixtoolset -y
powershell -ExecutionPolicy Bypass -File packaging/windows/build.ps1
powershell -ExecutionPolicy Bypass -File packaging/windows/build-msi.ps1
```

输出：

```text
packaging/windows/output/RustVNC-Setup-0.1.0.exe
packaging/windows/output/RustVNC.msi
```

MSI 是 per-user WiX 工程（`Product.wxs`），安装到
`%LOCALAPPDATA%\RustVNC`，不要求管理员权限。

## 发布

推 tag 即可触发 `.github/workflows/release.yml`：

```bash
git tag v0.2.0
git push origin v0.2.0
```

工作流会构建 EXE、MSI，计算 EXE 的 SHA-256，生成 `update.json`，并创建
GitHub Release（包含 macOS Agent zip）。客户端默认检查：

```text
https://github.com/civl/RustVNC/releases/latest/download/update.json
```

如果配置了 Windows 签名 secret，EXE/MSI 会先签名；否则会明确警告未签名。

安装目录：`%LOCALAPPDATA%\Programs\RustVNC`。

## 代码签名

默认不签名（本地开发友好），安装包与主程序都会在 `build.ps1` 里被调用
`sign.ps1` 检查一次。启用签名只需设置环境变量：

```powershell
$env:RUSTVNC_SIGN_CERT      = "C:\secrets\rustvnc.pfx"   # 或证书指纹（本机证书存储）
$env:RUSTVNC_SIGN_PASSWORD  = "…"                         # 用证书存储时省略
$env:RUSTVNC_SIGN_TIMESTAMP = "http://timestamp.digicert.com"  # 可省略
powershell -ExecutionPolicy Bypass -File packaging/windows/build.ps1
```

CI/release 里再设置 `RUSTVNC_REQUIRE_SIGN=1`：缺少证书时构建会**失败**而不是
静默产出未签名安装包（未签名安装包会被 SmartScreen 拦截，见 design.md 风险 R9）。

## 说明

- 安装包只包含客户端；macOS Agent 需单独安装 `.app`（见 `scripts/build-app.sh`）。
- 需 Windows 10 1809+ / x64；客户端使用 OpenGL（eframe/glow）。
- 首次启动会显示“首次使用”提示，引导填写 Mac 地址、端口和凭据。
- 建议在客户端“安全”中选择 SSH 隧道；首次连接新 Mac 需先点“探测指纹”，核对
  SHA256 后再信任（不会静默 `accept-new`）。
- 想用剪贴板/音频，需要在 Mac 上安装 Agent，并在客户端“增强通道”里配对。

## 发布所需 secrets

`release.yml`（tag 触发）使用以下 secrets，缺省时自动降级为未签名构建并给出 warning：

| Secret | 用途 |
|---|---|
| `WINDOWS_SIGN_CERT_BASE64` | pfx 的 base64 |
| `WINDOWS_SIGN_PASSWORD` | pfx 密码 |
| `MACOS_SIGN_CERT_BASE64` / `MACOS_SIGN_CERT_PASSWORD` | Developer ID Application 证书（.p12） |
| `MACOS_SIGN_IDENTITY` | 例如 `Developer ID Application: X (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID` / `MACOS_NOTARY_TEAM_ID` / `MACOS_NOTARY_PASSWORD` | `notarytool` 凭据（app-specific password） |
