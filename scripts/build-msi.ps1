# Build the Nullgate MSI end to end: release build -> sign exes -> GTK bundle ->
# wix build -> sign MSI.
#
# Prereqs (see docs/windows-packaging.md):
#   * GTK built/installed via gvsbuild (default C:\gtk).
#   * WiX 5 dotnet tool:  dotnet tool install --global wix --version "5.*"
#     plus the UI + Util extensions (this script adds them if missing).
#   * Signing: RELEASES ARE SIGNED BY DEFAULT. Keep artifact-signing-metadata.json
#     at the repo root (git-ignored; or point $env:ARTIFACT_SIGNING_METADATA at it)
#     plus the Trusted Signing client tools + Windows SDK and an authenticated Azure
#     session (az login). If the metadata is absent the build still succeeds but the
#     exes/MSI are UNSIGNED — do NOT ship that as a release (SmartScreen will warn).
#   * The NullgateDaemon service does NOT need to be stopped. The installed service
#     runs the exe under Program Files, not the one in target\release, so it never
#     locks the build output. (An earlier version of this note said otherwise.)
#
# Usage:  pwsh -File scripts\build-msi.ps1 [-Arch x86_64|arm64] [-GtkRoot ...] [-Version <ver>] [-SkipBuild]
#   -> target\wix\nullgate-<version>-windows-<arch>.msi
#
# ARM64 is cross-built from this same x86_64 host; see scripts\build-arm64.ps1 for the
# toolchain it needs (llvm-mingw + the MSYS2 GTK stack) and docs\windows-packaging.md
# for why it is built the way it is.
#
# Version defaults to the workspace version in Cargo.toml (single source of truth).

param(
    [ValidateSet("x86_64", "arm64")]
    [string]$Arch = "x86_64",
    [string]$GtkRoot,
    [string]$Version,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $line) { throw "could not read version from Cargo.toml" }
    $Version = $line.Matches[0].Groups[1].Value
}

# Per-arch: where GTK comes from, which target dirs the exes land in, and which of the
# Util extension's per-architecture custom-action binaries the MSI must reference.
if ($Arch -eq "arm64") {
    if (-not $GtkRoot) { $GtkRoot = "C:\gtk-arm64" }
    $wixArch = "arm64"
    $utilCA  = "Wix4UtilCA_A64"
    # The GUI is mingw-ABI (gnullvm) and the service binaries are MSVC — see build-arm64.ps1.
    $exePaths = @(
        (Join-Path $root "target\aarch64-pc-windows-gnullvm\release\nullgate.exe"),
        (Join-Path $root "target\aarch64-pc-windows-msvc\release\nullgate-daemon.exe"),
        (Join-Path $root "target\aarch64-pc-windows-msvc\release\nullgate-cli.exe")
    )
} else {
    if (-not $GtkRoot) { $GtkRoot = "C:\gtk" }
    $wixArch = "x64"
    $utilCA  = "Wix4UtilCA_X64"
    $exePaths = "nullgate.exe", "nullgate-daemon.exe", "nullgate-cli.exe" |
        ForEach-Object { Join-Path $root "target\release\$_" }
}

Write-Host "Building Nullgate MSI $Version ($Arch)" -ForegroundColor Cyan

$env:PATH = "$env:USERPROFILE\.dotnet\tools;$env:PATH"
if ($Arch -eq "x86_64") {
    # The x86_64 build links against gvsbuild directly; the arm64 build sets its own
    # (quite different) pkg-config environment inside build-arm64.ps1.
    $env:PKG_CONFIG_PATH = "$GtkRoot\lib\pkgconfig"
    $env:PATH = "$GtkRoot\bin;$env:PATH"
    $env:LIB = "$GtkRoot\lib;$env:LIB"
}

if ($SkipBuild) {
    Write-Host "[1/6] cargo build --release (SKIPPED -SkipBuild)" -ForegroundColor Cyan
} elseif ($Arch -eq "arm64") {
    Write-Host "[1/6] cross-building for ARM64" -ForegroundColor Cyan
    & "$root\scripts\build-arm64.ps1" -GtkRoot $GtkRoot
    if ($LASTEXITCODE -ne 0) { throw "arm64 build failed" }
} else {
    Write-Host "[1/6] cargo build --release" -ForegroundColor Cyan
    & cargo build --release -p ipn-gui -p ipn-daemon -p ipn-cli
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed. If it couldn't replace nullgate-daemon.exe, something is running it FROM THE BUILD TREE (e.g. cargo run -p ipn-daemon) - stop that. The installed service runs from Program Files and does not lock it."
    }
}

# Sign our exes in the target dirs FIRST, so both the MSI and the portable zip the
# bundle produces carry signed binaries.
Write-Host "[2/6] signing our exes (Azure Trusted Signing, if configured)" -ForegroundColor Cyan
& "$root\scripts\sign-artifacts.ps1" -Files $exePaths

Write-Host "[3/6] bundling the GTK runtime -> dist\nullgate-windows-$Arch" -ForegroundColor Cyan
& "$root\scripts\bundle-gtk-windows.ps1" -Arch $Arch -GtkRoot $GtkRoot -SkipBuild
if ($LASTEXITCODE -ne 0) { throw "bundling failed" }
$dist = Join-Path $root "dist\nullgate-windows-$Arch"

Write-Host "[4/6] generating the license RTF from LICENSE" -ForegroundColor Cyan
$licenseRtf = Join-Path $root "wix\license.rtf"
$plain = Get-Content -Raw (Join-Path $root "LICENSE")
# Minimal RTF: escape RTF metacharacters, map newlines to \par. Non-ASCII becomes
# \u escapes so the GPL's curly quotes etc. render correctly.
$escaped = $plain -replace '\\', '\\' -replace '\{', '\{' -replace '\}', '\}'
$sb = [System.Text.StringBuilder]::new()
[void]$sb.Append("{\rtf1\ansi\deff0{\fonttbl{\f0\fnil Segoe UI;}}\fs18`r`n")
foreach ($ch in $escaped.ToCharArray()) {
    $code = [int]$ch
    if ($ch -eq "`n") { [void]$sb.Append("\par`r`n") }
    elseif ($ch -eq "`r") { }
    elseif ($code -gt 127) { [void]$sb.Append("\u$code?") }
    else { [void]$sb.Append($ch) }
}
[void]$sb.Append("`r`n}")
Set-Content -Path $licenseRtf -Value $sb.ToString() -Encoding ASCII

Write-Host "[5/6] wix build" -ForegroundColor Cyan
# Ensure the UI + Util extensions are present at EXACTLY the installed wix engine
# version (add unconditionally — idempotent; a name-only "already present?" check
# is unsafe if a different version is registered).
$wixVer = ((& wix --version) -split '\+')[0].Trim()
foreach ($ext in "WixToolset.UI.wixext", "WixToolset.Util.wixext") {
    Write-Host "  ensuring extension $ext/$wixVer" -ForegroundColor DarkGray
    & wix extension add -g "$ext/$wixVer"
    if ($LASTEXITCODE -ne 0) { throw "wix extension add failed for $ext/$wixVer" }
}

$out = Join-Path $root "target\wix\nullgate-$Version-windows-$Arch.msi"
New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
& wix build -arch $wixArch "$root\wix\nullgate.wxs" `
    -ext WixToolset.UI.wixext -ext WixToolset.Util.wixext `
    -d DistDir="$dist" -d Version="$Version" -d LicenseRtf="$licenseRtf" `
    -d UtilCA="$utilCA" `
    -o $out
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

Write-Host "[6/6] signing the MSI (Azure Trusted Signing, if configured)" -ForegroundColor Cyan
& "$root\scripts\sign-artifacts.ps1" -Files @($out)

Write-Host ("Done -> {0}  ({1:N1} MB)" -f $out, ((Get-Item $out).Length / 1MB)) -ForegroundColor Green
Write-Host "Install (elevated):   msiexec /i `"$out`"" -ForegroundColor Green
Write-Host "Uninstall (elevated): msiexec /x `"$out`"" -ForegroundColor Green
