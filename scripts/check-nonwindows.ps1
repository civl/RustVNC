<#
.SYNOPSIS
    在 Windows 开发机上模拟"非 Windows 目标"的 clippy，抓只在该配置下出现的问题。

.DESCRIPTION
    CI 的 macOS job 会跑 `cargo clippy --workspace --all-targets -- -D warnings`。
    它抓得住一类本机永远看不见的问题：`#[cfg(windows)]` 里被引用的常量/函数，在
    非 Windows 下没人用，于是变成 dead code 而让 -D warnings 失败（本项目实际踩过
    两次：`POLL_INTERVAL_MS` 与 `spawn_listener`）。

    本机没有 macOS/Linux 的 C 工具链，`cargo clippy --target x86_64-unknown-linux-gnu`
    会卡在 cc-rs 上，所以这里用另一种办法：把指定模块复制进一个临时 crate，并把
    cfg 反转成"非 Windows 视角"——
        #[cfg(windows)]        -> #[cfg(any())]     （恒假，等价于不存在）
        #[cfg(not(windows))]   -> 直接删除该属性     （恒真）
        #[cfg(all(test, windows))] -> #[cfg(any())]
    然后对一个自包含的小 crate 跑 clippy -D warnings。它检查的是模块自身在
    非 Windows 下的死代码与 lint，这正是 CI 报错的那一类。

    能翻转的只有"自包含"模块：不引用 crate 内其他模块（`crate::foo`）。脚本会扫出
    所有含 windows cfg 的文件，并提示哪些没被覆盖，避免悄悄漏检。

.PARAMETER Modules
    要检查的模块名（src 下的文件名，不含 .rs）。默认覆盖单实例守卫与系统通知。

.PARAMETER Dependencies
    临时 crate 需要的依赖（模块里用到的外部 crate）。

.PARAMETER OutDir
    临时 crate 的落脚处。默认放在 %TEMP%，除非 -Keep 否则结束后删除。

.EXAMPLE
    pwsh -File scripts/check-nonwindows.ps1

.EXAMPLE
    pwsh -File scripts/check-nonwindows.ps1 -Modules single_instance,notify -Keep
#>
[CmdletBinding()]
param(
    [string[]]$Modules = @('notify', 'single_instance'),
    [hashtable]$Dependencies = @{ tracing = '0.1' },
    [string]$OutDir = (Join-Path $env:TEMP 'rustvnc-cfgcheck'),
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path $PSScriptRoot -Parent
$srcRoot = Join-Path $repo 'crates\rustvnc-client\src'
if (-not (Test-Path $srcRoot)) { throw "找不到客户端源码目录：$srcRoot" }

# 找出所有被 windows cfg 门控的文件，并提醒哪些没有被检查。
$gated = Get-ChildItem $srcRoot -Filter *.rs | Where-Object {
    Select-String -Path $_.FullName -Pattern 'cfg\(\s*(all\(\s*test\s*,\s*)?windows\s*\)|not\(\s*windows\s*\)' -Quiet
}
$covered = @($Modules | ForEach-Object { "$_.rs" })
foreach ($file in $gated) {
    if ($file.Name -notin $covered) {
        Write-Warning "$($file.Name) 含 windows 专属 cfg，但不在 -Modules 里；若它自包含，请加进去否则这类问题会漏检。"
    }
}

if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $OutDir 'src') -Force | Out-Null

foreach ($module in $Modules) {
    $source = Join-Path $srcRoot "$module.rs"
    if (-not (Test-Path $source)) { throw "找不到模块 $module（$source）" }

    # cfg 反转：windows 恒假、not(windows) 恒真。
    $inverted = foreach ($line in (Get-Content $source)) {
        if ($line.Trim() -eq '#[cfg(not(windows))]') { continue }
        $line -replace 'all\(\s*test\s*,\s*windows\s*\)', 'any()' -replace 'cfg\(\s*windows\s*\)', 'cfg(any())'
    }
    Set-Content -Path (Join-Path $OutDir "src\$module.rs") -Value $inverted -Encoding utf8
}

$lib = ($Modules | ForEach-Object { "pub mod $_;" }) -join "`n"
Set-Content -Path (Join-Path $OutDir 'src\lib.rs') -Value $lib -Encoding utf8

$dependencyLines = ($Dependencies.GetEnumerator() | ForEach-Object { "$($_.Key) = `"$($_.Value)`"" }) -join "`n"
$manifest = @"
[package]
name = "rustvnc-cfgcheck"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
$dependencyLines
"@
Set-Content -Path (Join-Path $OutDir 'Cargo.toml') -Value $manifest -Encoding utf8

Push-Location $OutDir
try {
    # `--offline` 必须放在 `--` 之前（那是 cargo 的开关），否则会被当成 rustc 参数。
    $arguments = @('clippy', '--all-targets', '--offline', '--', '-D', 'warnings')
    $output = & cargo @arguments 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0 -and (($output -join "`n") -match 'no matching package|offline mode')) {
        Write-Host '本地缓存不足以离线解析依赖，联网重试…'
        $retry = @($arguments | Where-Object { $_ -ne '--offline' })
        $output = & cargo @retry 2>&1
        $code = $LASTEXITCODE
    }
} finally {
    Pop-Location
}

if ($code -ne 0) {
    $output | Select-Object -Last 40 | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host "非 Windows 视图检查失败：$($Modules -join ', ')（上面是 -D warnings 的报错）" -ForegroundColor Red
    if (-not $Keep) { Remove-Item $OutDir -Recurse -Force -ErrorAction SilentlyContinue }
    exit 1
}

Write-Host "非 Windows 视图检查通过：$($Modules -join ', ')" -ForegroundColor Green
if ($Keep) {
    Write-Host "临时 crate 保留在 $OutDir"
} else {
    Remove-Item $OutDir -Recurse -Force -ErrorAction SilentlyContinue
}
exit 0
