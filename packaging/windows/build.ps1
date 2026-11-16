$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location $root
try {
    cargo build --release -p rustvnc-client
    $stage = Join-Path $PSScriptRoot "stage"
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item (Join-Path $root "target\release\rustvnc-client.exe") $stage -Force
    $iscc = Get-Command "iscc.exe" -ErrorAction SilentlyContinue
    if (-not $iscc) {
        Write-Host "Inno Setup 6 not found. Install it, then re-run:"
        Write-Host "  winget install JRSoftware.InnoSetup"
        Write-Host "Staged EXE: $stage\rustvnc-client.exe"
        exit 2
    }
    & $iscc.Source (Join-Path $PSScriptRoot "rustvnc.iss")
    Write-Host "Installer written to packaging\windows\output"
} finally {
    Pop-Location
}
