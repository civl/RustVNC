$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location $root
try {
    cargo build --release -p rustvnc-client
    $stage = Join-Path $PSScriptRoot "stage"
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item (Join-Path $root "target\release\rustvnc-client.exe") $stage -Force
    $iscc = Get-Command "iscc.exe" -ErrorAction SilentlyContinue
    $isccPath = if ($iscc) { $iscc.Source } else { $null }
    if (-not $isccPath) {
        foreach ($candidate in @(
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
            "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
        )) {
            if (Test-Path $candidate) { $isccPath = $candidate; break }
        }
    }
    if (-not $isccPath) {
        Write-Host "Inno Setup 6 not found. Install it, then re-run:"
        Write-Host "  winget install JRSoftware.InnoSetup"
        Write-Host "Staged EXE: $stage\rustvnc-client.exe"
        exit 2
    }
    & $isccPath (Join-Path $PSScriptRoot "rustvnc.iss")
    Write-Host "Installer written to packaging\windows\output"
} finally {
    Pop-Location
}
