# RustVNC 一键验收（Windows）
#
# 跑完所有能在 Windows 上自动验证的项目，并把结果写进
# target/acceptance/report-<时间>.txt。
#
#   powershell -ExecutionPolicy Bypass -File scripts/acceptance.ps1
#
# 可选：指定一台可连的 Mac，会额外跑协议探针（只读，不提交凭据）
#
#   $env:RUSTVNC_MAC_HOST = "192.168.1.20"
#   $env:RUSTVNC_MAC_PORT = "5900"     # 可选，默认 5900
#   powershell -ExecutionPolicy Bypass -File scripts/acceptance.ps1
#
# 退出码：0 = 全部通过；1 = 有检查失败。

[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Continue"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $root
$results = New-Object System.Collections.Generic.List[object]

function Invoke-Step {
    param(
        [string]$Name,
        [string]$Command
    )
    Write-Host ""
    Write-Host "== $Name" -ForegroundColor Cyan
    Write-Host "   $Command" -ForegroundColor DarkGray
    $started = Get-Date
    $output = & pwsh -NoProfile -Command $Command 2>&1
    $code = $LASTEXITCODE
    $elapsed = (Get-Date) - $started
    $output | ForEach-Object { Write-Host $_ }
    $results.Add([pscustomobject]@{
            Step    = $Name
            Passed  = ($code -eq 0)
            Exit    = $code
            Seconds = [math]::Round($elapsed.TotalSeconds, 1)
        })
    if ($code -ne 0) {
        Write-Host "   FAILED (exit $code)" -ForegroundColor Red
    }
}

Write-Host "RustVNC acceptance — $(Get-Date -Format o)" -ForegroundColor Green
Write-Host "workspace: $root"

Invoke-Step "format" "cargo fmt --all --check"

# The root package is the macOS agent, so `--workspace` cannot build on Windows.
$windowsPackages = "-p rustvnc-client -p rustvnc-agent-client -p vnc-rs -p rfb-probe"
Invoke-Step "clippy (windows packages)" "cargo clippy $windowsPackages --all-targets -- -D warnings"
Invoke-Step "tests: protocol engine (vendor/vnc-rs)" "cargo test -p vnc-rs"
Invoke-Step "tests: probe" "cargo test -p rfb-probe"
Invoke-Step "tests: client (session/encoding/idle)" "cargo test -p rustvnc-client"
Invoke-Step "tests: agent channel (QUIC loopback + Opus)" "cargo test -p rustvnc-agent-client"

# Optimised builds catch a whole class of bug that debug hides (the 16 bpp
# "all black" regression was an integer overflow that panicked in debug but
# wrapped silently in release).
Invoke-Step "tests: release mode (client + engine + agent)" "cargo test --release -p rustvnc-client -p vnc-rs -p rustvnc-agent-client"

if (-not $SkipBuild) {
    Invoke-Step "release build: client" "cargo build --release -p rustvnc-client"
}

# 真机相关的环境变量：主机名来自 RUSTVNC_MAC_HOST，凭据来自用户级 MACOS_USERNAME /
# MACOS_PASSWORD（本项目约定）。凭据只在本进程内传递给探针，不落盘、不打印。
$macHost = $env:RUSTVNC_MAC_HOST
if (-not $macHost) { $macHost = [Environment]::GetEnvironmentVariable('MACOS_HOST','User') }
$macUser = $env:MACOS_USERNAME
if (-not $macUser) { $macUser = [Environment]::GetEnvironmentVariable('MACOS_USERNAME','User') }
if (-not $macUser) {
    # 没设用户名时用已保存配置里的账号，免得再要一份重复的凭据。
    $profilePath = Join-Path $env:APPDATA 'RustVNC\client\profiles.json'
    if (Test-Path $profilePath) {
        $saved = (Get-Content $profilePath -Raw | ConvertFrom-Json).profiles | Select-Object -First 1
        if ($saved) { $macUser = $saved.config.user }
    }
}
$macPassword = [Environment]::GetEnvironmentVariable('MACOS_PASSWORD','User')

if ($macHost) {
    $port = if ($env:RUSTVNC_MAC_PORT) { $env:RUSTVNC_MAC_PORT } else { "5900" }
    # Read-only: version string + offered security types, no credentials.
    Invoke-Step "probe real Mac ($($macHost):$port)" `
        "cargo run --quiet -p rfb-probe -- --inspect --host $macHost --port $port"
    if ($macUser -and $macPassword) {
        # 认证后跑 5 秒，验证账号密码确实可用（凭据经环境变量传给探针）。
        $env:VNC_USERNAME = $macUser
        $env:VNC_PASSWORD = $macPassword
        Invoke-Step "probe auth + 5s stream ($($macHost):$port)" `
            "cargo run --quiet -p rfb-probe -- --host $macHost --port $port --auth ard --user $macUser --seconds 5"
        Remove-Item Env:\VNC_PASSWORD -ErrorAction SilentlyContinue
        Remove-Item Env:\VNC_USERNAME -ErrorAction SilentlyContinue
    } else {
        Write-Host "提示：未设置 MACOS_USERNAME / MACOS_PASSWORD，跳过登录态探针。" -ForegroundColor Yellow
    }
} else {
    Write-Host ""
    Write-Host "跳过真机探针（未设置 RUSTVNC_MAC_HOST 或用户级 MACOS_HOST）" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "================ 汇总 ================" -ForegroundColor Green
$results | Format-Table -AutoSize | Out-String | Write-Host

$failed = $results | Where-Object { -not $_.Passed }
$reportDir = Join-Path $root "target\acceptance"
New-Item -ItemType Directory -Force -Path $reportDir | Out-Null
$report = Join-Path $reportDir ("report-{0}.txt" -f (Get-Date -Format "yyyyMMdd-HHmmss"))
@(
    "RustVNC acceptance report"
    "generated: $(Get-Date -Format o)"
    "host: $env:COMPUTERNAME / $(Get-CimInstance Win32_OperatingSystem | Select-Object -ExpandProperty Caption)"
    ""
    ($results | Format-Table -AutoSize | Out-String)
    "result: $(if ($failed) { 'FAILED' } else { 'PASSED' })"
) | Set-Content -Path $report -Encoding utf8
Write-Host "report: $report"

Pop-Location
if ($failed) { exit 1 } else { exit 0 }
