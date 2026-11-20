<#
.SYNOPSIS
    在本机跑解析器 fuzz（M3 的"24 小时"验收项）。

.DESCRIPTION
    cargo-fuzz 在 Windows/MSVC 上需要 ASan 运行库在 PATH 里，否则 fuzz 目标会以
    STATUS_DLL_NOT_FOUND (0xc0000135) 退出。本脚本负责找到那个目录、装好 nightly 与
    cargo-fuzz，再按目标跑指定小时数。

    目标：
      apple_metadata  Apple 伪编码（1104/1105/1107/1109/1110）解析
      control_msg     Agent 控制通道 postcard 解码

.EXAMPLE
    # 两个目标各跑 24 小时（并行，约占 2 个核）
    .\scripts\fuzz-local.ps1 -Hours 24

.EXAMPLE
    # 快速验证脚本本身没问题
    .\scripts\fuzz-local.ps1 -Hours 0.02
#>
[CmdletBinding()]
param(
    [double]$Hours = 24,
    [string[]]$Targets = @('apple_metadata', 'control_msg'),
    [string]$RustupToolchain = 'nightly'
)

$ErrorActionPreference = 'Stop'
$fuzzDir = Join-Path $PSScriptRoot '..\fuzz'
$seconds = [int]($Hours * 3600)

function Resolve-AsanRuntime {
    # 先看 rustup 的 LLVM，再看 Visual Studio 各版本。
    $sysroot = & rustc "+$RustupToolchain" --print sysroot 2>$null
    $candidates = @()
    if ($sysroot) {
        $candidates += Get-ChildItem -Path $sysroot -Filter 'clang_rt.asan_dynamic-x86_64.dll' -Recurse -ErrorAction SilentlyContinue
    }
    foreach ($root in @(
            "$env:ProgramFiles\Microsoft Visual Studio",
            "${env:ProgramFiles(x86)}\Microsoft Visual Studio")) {
        if (Test-Path $root) {
            $candidates += Get-ChildItem -Path $root -Filter 'clang_rt.asan_dynamic-x86_64.dll' -Recurse -ErrorAction SilentlyContinue
        }
    }
    $hit = $candidates | Where-Object { $_.FullName -match 'Hostx64\\x64' } | Select-Object -First 1
    if (-not $hit) { throw '找不到 clang_rt.asan_dynamic-x86_64.dll：请安装 VS Build Tools 的 C++ 工具链。' }
    return $hit.DirectoryName
}

$env:PATH = "$(Resolve-AsanRuntime);$env:PATH"
Write-Host "ASan 运行库: $($env:PATH.Split(';')[0])"

& rustup toolchain list | Select-String -Pattern "^$RustupToolchain" -Quiet | Out-Null
if (-not (& rustup toolchain list | Select-String -Pattern "^$RustupToolchain")) {
    & rustup toolchain install $RustupToolchain --profile minimal
}
if (-not (Get-Command cargo-fuzz -ErrorAction SilentlyContinue)) {
    & cargo "+$RustupToolchain" install cargo-fuzz --locked
}

Push-Location $fuzzDir
try {
    $jobs = foreach ($target in $Targets) {
        $log = Join-Path $fuzzDir "fuzz-$target-$(Get-Date -Format 'yyyyMMdd-HHmmss').log"
        Write-Host "启动 $target：$seconds 秒，日志 $log"
        Start-Process -FilePath 'cargo' `
            -ArgumentList @("+$RustupToolchain", 'fuzz', 'run', $target, '--', "-max_total_time=$seconds", '-rss_limit_mb=4096') `
            -RedirectStandardOutput $log -RedirectStandardError "$log.err" `
            -PassThru -NoNewWindow
    }
    Write-Host "已启动 $($jobs.Count) 个目标。看日志：fuzz\fuzz-*.log"
    Write-Host "崩溃语料在 fuzz\artifacts\<target>\；有崩溃时进程会写 0 长度以外的输入文件。"
} finally {
    Pop-Location
}
