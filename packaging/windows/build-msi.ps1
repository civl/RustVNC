# Build a per-user WiX MSI from the already-staged client executable.
# Requires WiX Toolset 3.x (choco install wixtoolset -y).
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$stage = Join-Path $PSScriptRoot "stage"
$output = Join-Path $PSScriptRoot "output"
$obj = Join-Path $PSScriptRoot "obj"

if (-not (Test-Path (Join-Path $stage "rustvnc-client.exe"))) {
    throw "missing staged client; run build.ps1 first"
}

function Find-WixTool([string]$Name) {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        "${env:ProgramFiles(x86)}\WiX Toolset v3.14\bin\$Name",
        "${env:ProgramFiles}\WiX Toolset v3.14\bin\$Name",
        "${env:ProgramFiles(x86)}\WiX Toolset v3.11\bin\$Name"
    )
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

$candle = Find-WixTool "candle.exe"
$light = Find-WixTool "light.exe"
if (-not $candle -or -not $light) {
    throw "WiX Toolset not found. Run: choco install wixtoolset -y"
}

New-Item -ItemType Directory -Force -Path $output, $obj | Out-Null
Push-Location $PSScriptRoot
try {
    & $candle -arch x64 "-dStageDir=$stage" Product.wxs -out (Join-Path $obj "Product.wixobj")
    if ($LASTEXITCODE -ne 0) { throw "candle failed" }
    & $light -out (Join-Path $output "RustVNC.msi") (Join-Path $obj "Product.wixobj")
    if ($LASTEXITCODE -ne 0) { throw "light failed" }
} finally {
    Pop-Location
}

$msi = Join-Path $output "RustVNC.msi"
& (Join-Path $PSScriptRoot "sign.ps1") -Path $msi -Description "RustVNC MSI"
Write-Host "MSI: $msi"
