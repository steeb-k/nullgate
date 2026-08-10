<#
.SYNOPSIS
  One-time setup for the Android soak harness: builds the desktop binaries,
  starts an isolated "soaknet" daemon (own socket + data dir, no TUN), boots
  the emulator with the app, and walks the emulator through joining soaknet
  (auto-approving the join). Idempotent — safe to re-run; it skips whatever
  already exists.

  The one manual part is on the emulator screen: tap Join, focus the ticket
  field (this script offers to type the ticket for you over adb), confirm the
  SAS emoji, and grant the VPN consent dialog. That happens once; every soak
  run after this is fully unattended.

.PARAMETER SkipAppBuild  Don't rebuild the APK; install/launch what's there.
.EXAMPLE
  pwsh -File scripts\soak\setup-soak.ps1
#>
param([switch]$SkipAppBuild)

. (Join-Path $PSScriptRoot 'soaklib.ps1')

# ------------------------------------------------------- desktop half ---------
Step 'Building desktop binaries (ipn-daemon + ipn-cli, debug)'
Push-Location $script:RepoRoot
try {
    cargo build -p ipn-daemon -p ipn-cli
    if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed' }
} finally { Pop-Location }

New-Item -ItemType Directory -Force $script:SoakRoot | Out-Null
Step 'Starting the isolated soak daemon'
Start-SoakDaemon

$status = Invoke-Cli status
if ($status.Out -match 'no network on this device') {
    Step 'Creating throwaway network "soaknet"'
    $r = Invoke-Cli create soaknet
    if (-not $r.Ok) { Fail "create failed: $($r.Out)" }
} else {
    Step 'Soak daemon already has a network — keeping it'
}

# ------------------------------------------------------- emulator half --------
Step 'Booting emulator + installing the app (via scripts\run-android.ps1)'
$raArgs = @('-File', (Join-Path $script:RepoRoot 'scripts\run-android.ps1'))
if ($SkipAppBuild) { $raArgs += '-SkipBuild' }
pwsh @raArgs
if ($LASTEXITCODE -ne 0) { Fail 'run-android.ps1 failed' }

# The join itself is fully automated (UI driven over adb, VPN consent
# pre-granted via appops, approval from the daemon side) — see join-emulator.ps1.
pwsh -File (Join-Path $PSScriptRoot 'join-emulator.ps1')
if ($LASTEXITCODE -ne 0) { Fail 'automated join failed — see output above' }

Write-Host ''
Step 'Setup complete'
Write-Host 'Run a scenario:  pwsh -File scripts\soak\run-soak.ps1 -Scenario blackhole   (see README.md)' -ForegroundColor Green
