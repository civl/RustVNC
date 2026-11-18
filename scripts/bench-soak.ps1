<#
.SYNOPSIS
    长时间运行（soak）采样：盯住真实客户端的 CPU、内存、句柄与线程，判断是否泄漏或失控。

.DESCRIPTION
    启动（或附着到）rustvnc-client，每 N 秒采样一次，写 CSV，结束时给出：
      - CPU 均值/峰值（按逻辑核心数折算成"单核百分比"与"整机百分比"）
      - 工作集/私有内存的起止值、峰值、每小时增长
      - 内存泄漏趋势（对时间的线性回归斜率）
      - 句柄与线程数的变化量
    并按阈值判定 PASS/FAIL，产出 summary.json。

    默认 480 分钟（8 小时）；`-Minutes 2` 可做冒烟。

.EXAMPLE
    # 无人值守 8 小时：自动连接已保存的配置 "Mac Mini M4"
    .\scripts\bench-soak.ps1 -Minutes 480 -Connect "Mac Mini M4"

.EXAMPLE
    # 只盯已经开着的客户端（先手动连好），跑 30 分钟
    .\scripts\bench-soak.ps1 -Minutes 30 -Attach
#>
[CmdletBinding()]
param(
    [double]$Minutes = 480,
    [int]$IntervalSeconds = 10,
    [string]$Exe = "$PSScriptRoot\..\target\release\rustvnc-client.exe",
    # 已保存的连接配置名；由客户端 --connect 参数自动连接。
    [string]$Connect,
    # 不启动新进程，直接采样已运行的客户端。
    [switch]$Attach,
    [string]$OutDir = "$PSScriptRoot\..\target\soak",
    # 阈值（默认值来自 design.md 的预算）
    [double]$MaxMeanCpuPercent = 15,
    [double]$MaxRssGrowthMbPerHour = 20,
    [int]$MaxHandleGrowth = 50
)

$ErrorActionPreference = 'Stop'
$processName = 'rustvnc-client'

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$csvPath = Join-Path $OutDir "soak-$stamp.csv"
$summaryPath = Join-Path $OutDir "soak-$stamp.json"

function Get-ClientProcess {
    Get-Process -Name $processName -ErrorAction SilentlyContinue |
        Sort-Object StartTime -Descending |
        Select-Object -First 1
}

$proc = $null
if ($Attach) {
    $proc = Get-ClientProcess
    if (-not $proc) { throw "没有找到正在运行的 $processName；先用 -Connect 启动，或手动打开客户端。" }
    Write-Host "附着到 PID $($proc.Id)（$($proc.Path)）"
} else {
    if (-not (Test-Path $Exe)) { throw "找不到客户端：$Exe（先 cargo build --release -p rustvnc-client）" }
    $arguments = @()
    if ($Connect) { $arguments += @('--connect', $Connect) }
    Write-Host "启动 $Exe $($arguments -join ' ')"
    $proc = Start-Process -FilePath $Exe -ArgumentList $arguments -PassThru
    Start-Sleep -Seconds 3
}

$logicalCores = [Environment]::ProcessorCount
$started = Get-Date
$deadline = $started.AddMinutes($Minutes)
$lastCpu = $proc.TotalProcessorTime
$lastAt = Get-Date
$samples = New-Object System.Collections.Generic.List[object]

Write-Host ("采样开始：每 {0}s 一次，计划 {1} 分钟，{2} 逻辑核心" -f $IntervalSeconds, $Minutes, $logicalCores)
Write-Host ("输出：{0}" -f $csvPath)

while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds $IntervalSeconds
    $proc.Refresh()
    if ($proc.HasExited) {
        Write-Warning "客户端已退出（exit code $($proc.ExitCode)）；提前结束采样。"
        break
    }
    $now = Get-Date
    $cpu = $proc.TotalProcessorTime
    $elapsed = ($now - $lastAt).TotalSeconds
    $cpuPercent = if ($elapsed -gt 0) {
        (($cpu - $lastCpu).TotalSeconds / $elapsed) * 100
    } else { 0 }
    $lastCpu = $cpu
    $lastAt = $now

    $sample = [pscustomobject]@{
        time_utc      = $now.ToUniversalTime().ToString('o')
        minutes       = [math]::Round(($now - $started).TotalMinutes, 2)
        cpu_percent   = [math]::Round($cpuPercent, 2)
        rss_mb        = [math]::Round($proc.WorkingSet64 / 1MB, 1)
        private_mb    = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
        handles       = $proc.HandleCount
        threads       = $proc.Threads.Count
        cpu_seconds   = [math]::Round($cpu.TotalSeconds, 1)
    }
    $samples.Add($sample)
    $sample | Export-Csv -Path $csvPath -Append -NoTypeInformation -Encoding utf8
}

$finished = Get-Date
if ($samples.Count -lt 2) {
    throw "采样点不足（$($samples.Count) 个），无法给出结论。"
}

# 稳态基线：跳过开头 10% 的采样点（JIT、字体缓存、GPU 资源都在那时分配）。
$skip = [math]::Max(0, [int][math]::Floor($samples.Count * 0.1))
$steady = if ($samples.Count - $skip -ge 2) { $samples[$skip..($samples.Count - 1)] } else { $samples }

# 线性回归：内存随时间的变化斜率，用来区分"有波动的稳态"与"持续增长"。
$n = $steady.Count
$xs = $steady | ForEach-Object { $_.minutes }
$ys = $steady | ForEach-Object { $_.rss_mb }
$meanX = ($xs | Measure-Object -Average).Average
$meanY = ($ys | Measure-Object -Average).Average
$num = 0.0; $den = 0.0
for ($i = 0; $i -lt $n; $i++) {
    $num += ($xs[$i] - $meanX) * ($ys[$i] - $meanY)
    $den += [math]::Pow($xs[$i] - $meanX, 2)
}
$slopeMbPerMinute = if ($den -gt 0) { $num / $den } else { 0 }
$slopeMbPerHour = $slopeMbPerMinute * 60

$cpuStats = $samples | Measure-Object -Property cpu_percent -Average -Maximum
$rssStats = $samples | Measure-Object -Property rss_mb -Minimum -Maximum -Average
$first = $steady[0]
$last = $steady[$n - 1]
$rssGrowth = $last.rss_mb - $first.rss_mb
$hours = ($finished - $started).TotalHours
$growthPerHour = if ($hours -gt 0) { $rssGrowth / $hours } else { 0 }
$handleDelta = $last.handles - $first.handles

$failures = @()
if ($cpuStats.Average -gt $MaxMeanCpuPercent) {
    $failures += "CPU 均值 $([math]::Round($cpuStats.Average,1))% 超过预算 $MaxMeanCpuPercent%"
}
if ($growthPerHour -gt $MaxRssGrowthMbPerHour) {
    $failures += "内存每小时增长 $([math]::Round($growthPerHour,1)) MB 超过预算 $MaxRssGrowthMbPerHour MB"
}
# 短窗口上做回归没有意义：1 分钟的风吹草动能外推成几十 MB/h。要求足够长的
# 采样窗口，并且绝对增长也要过阈值，才判定为泄漏。
$leakMinMinutes = 30
$absoluteGrowthGate = 30
if (($finished - $started).TotalMinutes -lt $leakMinMinutes) {
    Write-Host ("注意：窗口不足 {0} 分钟，跳过泄漏斜率判定（只报数据）。" -f $leakMinMinutes) -ForegroundColor Yellow
} elseif ($slopeMbPerHour -gt $MaxRssGrowthMbPerHour -and $rssGrowth -gt $absoluteGrowthGate) {
    $failures += "内存回归斜率 $([math]::Round($slopeMbPerHour,1)) MB/h（累计 $([math]::Round($rssGrowth,1)) MB）超过预算（疑似泄漏）"
}
if ($handleDelta -gt $MaxHandleGrowth) {
    $failures += "句柄增加 $handleDelta 超过预算 $MaxHandleGrowth"
}

$summary = [pscustomobject]@{
    started_utc          = $started.ToUniversalTime().ToString('o')
    finished_utc         = $finished.ToUniversalTime().ToString('o')
    duration_minutes     = [math]::Round(($finished - $started).TotalMinutes, 1)
    samples              = $n
    interval_seconds     = $IntervalSeconds
    logical_cores        = $logicalCores
    cpu_percent_mean     = [math]::Round($cpuStats.Average, 2)
    cpu_percent_max      = [math]::Round($cpuStats.Maximum, 2)
    rss_mb_first         = $first.rss_mb
    rss_mb_last          = $last.rss_mb
    rss_mb_peak          = $rssStats.Maximum
    rss_growth_mb        = [math]::Round($rssGrowth, 1)
    rss_growth_mb_per_hour = [math]::Round($growthPerHour, 1)
    rss_slope_mb_per_hour  = [math]::Round($slopeMbPerHour, 1)
    handles_first        = $first.handles
    handles_last         = $last.handles
    threads_first        = $first.threads
    threads_last         = $last.threads
    passed               = ($failures.Count -eq 0)
    failures             = $failures
    csv                  = $csvPath
}
$summary | ConvertTo-Json -Depth 4 | Set-Content -Path $summaryPath -Encoding utf8

Write-Host ""
Write-Host "==== soak 结果（$([math]::Round(($finished - $started).TotalMinutes,1)) 分钟，$n 个采样点）===="
Write-Host ("CPU   均值 {0,6}%  峰值 {1,6}%   （{2} 核心）" -f $summary.cpu_percent_mean, $summary.cpu_percent_max, $logicalCores)
Write-Host ("内存  起 {0,7} MB  终 {1,7} MB  峰 {2,7} MB" -f $summary.rss_mb_first, $summary.rss_mb_last, $summary.rss_mb_peak)
Write-Host ("      增长 {0,6} MB（{1} MB/h）  回归斜率 {2} MB/h" -f $summary.rss_growth_mb, $summary.rss_growth_mb_per_hour, $summary.rss_slope_mb_per_hour)
Write-Host ("句柄  {0} -> {1}    线程 {2} -> {3}" -f $summary.handles_first, $summary.handles_last, $summary.threads_first, $summary.threads_last)
Write-Host ""
if ($summary.passed) {
    Write-Host "PASS：全部阈值内" -ForegroundColor Green
} else {
    Write-Host "FAIL：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
}
Write-Host "汇总：$summaryPath"
Write-Host "CSV ：$csvPath"

if (-not $Attach -and $proc -and -not $proc.HasExited) {
    Write-Host "关闭客户端（soak 结束）"
    $proc.CloseMainWindow() | Out-Null
    Start-Sleep -Seconds 2
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}

if (-not $summary.passed) { exit 1 }
