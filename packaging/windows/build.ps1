param(
    [string]$Version = "0.1.0"
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location $root
try {
    cargo build --release -p rustvnc-client
    $stage = Join-Path $PSScriptRoot "stage"
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item (Join-Path $root "target\release\rustvnc-client.exe") $stage -Force

    # Sign the executable before packaging: the installer embeds it, and the
    # signature has to survive the install. Opt-in via RUSTVNC_SIGN_CERT.
    & (Join-Path $PSScriptRoot "sign.ps1") -Path (Join-Path $stage "rustvnc-client.exe") -Description "RustVNC Client"

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
    & $isccPath "/DMyAppVersion=$Version" (Join-Path $PSScriptRoot "rustvnc.iss")
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }

    # Sign the installer itself, otherwise SmartScreen blocks the download.
    $output = Join-Path $PSScriptRoot "output"
    if (Test-Path $output) {
        Get-ChildItem $output -Filter "*.exe" | ForEach-Object {
            & (Join-Path $PSScriptRoot "sign.ps1") -Path $_.FullName -Description "RustVNC Client Installer"
        }
    }
    Write-Host "Installer written to packaging\windows\output"
} finally {
    Pop-Location
}
