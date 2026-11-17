# Authenticode-sign a Windows binary, or explain precisely why it was skipped.
#
# Signing is opt-in through environment variables so a local build keeps
# working without a certificate, and CI can fail loudly when it was supposed to
# sign but the secret is missing:
#
#   $env:RUSTVNC_SIGN_CERT      = "C:\secrets\codesign.pfx"      # or a thumbprint
#   $env:RUSTVNC_SIGN_PASSWORD  = "…"                            # omit for a store cert
#   $env:RUSTVNC_SIGN_TIMESTAMP = "http://timestamp.digicert.com"
#   $env:RUSTVNC_REQUIRE_SIGN   = "1"                            # CI: fail if it cannot sign
#
# Unsigned installers are the reason SmartScreen blocks the download with "this
# app can't run on your PC"; see risk R9 in design.md.
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [string]$Description = "RustVNC"
)

$ErrorActionPreference = "Stop"

function Find-SignTool {
    $cmd = Get-Command "signtool.exe" -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    if (Test-Path $kits) {
        $candidate = Get-ChildItem $kits -Directory |
            Sort-Object Name -Descending |
            Where-Object { Test-Path (Join-Path $_.FullName "x64\signtool.exe") } |
            Select-Object -First 1
        if ($candidate) { return Join-Path $candidate.FullName "x64\signtool.exe" }
    }
    return $null
}

if (-not (Test-Path $Path)) {
    throw "sign.ps1: file not found: $Path"
}

$cert = $env:RUSTVNC_SIGN_CERT
$require = $env:RUSTVNC_REQUIRE_SIGN -eq "1"

if ([string]::IsNullOrEmpty($cert)) {
    if ($require) {
        throw "RUSTVNC_REQUIRE_SIGN=1 but RUSTVNC_SIGN_CERT is not set: refusing to ship an unsigned build."
    }
    Write-Host "not signing $Path (RUSTVNC_SIGN_CERT not set) - SmartScreen will warn on download"
    exit 0
}

$signTool = Find-SignTool
if (-not $signTool) {
    if ($require) {
        throw "RUSTVNC_REQUIRE_SIGN=1 but signtool.exe was not found (install the Windows SDK)."
    }
    Write-Host "not signing $Path (signtool.exe not found)"
    exit 0
}

$timestamp = if ([string]::IsNullOrEmpty($env:RUSTVNC_SIGN_TIMESTAMP)) {
    "http://timestamp.digicert.com"
} else {
    $env:RUSTVNC_SIGN_TIMESTAMP
}

$arguments = @("sign", "/fd", "SHA256", "/tr", $timestamp, "/td", "SHA256", "/d", $Description)
if (Test-Path $cert) {
    $arguments += @("/f", $cert)
    if (-not [string]::IsNullOrEmpty($env:RUSTVNC_SIGN_PASSWORD)) {
        $arguments += @("/p", $env:RUSTVNC_SIGN_PASSWORD)
    }
} else {
    # Not a file: treat it as a certificate thumbprint in the local store.
    $arguments += @("/sha1", $cert)
}
$arguments += $Path

& $signTool @arguments
if ($LASTEXITCODE -ne 0) { throw "signtool failed with exit code $LASTEXITCODE" }

& $signTool verify /pa /v $Path
if ($LASTEXITCODE -ne 0) { throw "signature verification failed for $Path" }

Write-Host "signed and verified: $Path"
