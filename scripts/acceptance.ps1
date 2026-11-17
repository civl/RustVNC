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

if ($env:RUSTVNC_MAC_HOST) {
    $port = if ($env:RUSTVNC_MAC_PORT) { $env:RUSTVNC_MAC_PORT } else { "5900" }
    # Read-only: version string + offered security types, no credentials.
    Invoke-Step "probe real Mac ($($env:RUSTVNC_MAC_HOST):$port)" `
        "cargo run --quiet -p rfb-probe -- --inspect --host $($env:RUSTVNC_MAC_HOST) --port $port"
} else {
    Write-Host ""
    Write-Host "跳过真机探针（未设置 RUSTVNC_MAC_HOST）" -ForegroundColor Yellow
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
