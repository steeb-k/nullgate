<#
.SYNOPSIS
  Fully automated: join the emulator to the soak daemon's network with zero
  human interaction. Drives the app's UI over adb (uiautomator), pre-grants the
  VPN consent (ACTIVATE_VPN appop — emulator/adb only), and approves the join
  from the daemon side. Called by setup-soak.ps1; safe to run standalone.

.PARAMETER FreshNetwork  Dissolve the current soak network and start a new one
                         (also clears the app's data first). Use when the roster
                         state is suspect.
.EXAMPLE
  pwsh -File scripts\soak\join-emulator.ps1 -FreshNetwork
#>
param([switch]$FreshNetwork)

. (Join-Path $PSScriptRoot 'soaklib.ps1')

# ---------------------------------------------------------- UI helpers --------

function Get-UiDump {
    AdbSoft shell uiautomator dump /sdcard/ui.xml | Out-Null
    AdbSoft shell cat /sdcard/ui.xml
}

# Center of the first node whose exact attribute matches (text= or class=).
function Find-UiCenter([string]$Xml, [string]$Attr, [string]$Value) {
    $pattern = '<node[^>]*' + $Attr + '="' + [regex]::Escape($Value) + '"[^>]*>'
    $m = [regex]::Match($Xml, $pattern)
    if (-not $m.Success) { return $null }
    if ($m.Value -match 'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"') {
        return @{
            X = [int](([int]$Matches[1] + [int]$Matches[3]) / 2)
            Y = [int](([int]$Matches[2] + [int]$Matches[4]) / 2)
        }
    }
    $null
}

# Save what was actually on screen next to the soak state, then bail — a UI
# automation failure without the dump is undebuggable after the fact.
function Fail-WithUiDump([string]$Msg) {
    $f = Join-Path $script:SoakRoot 'last-ui.xml'
    Get-UiDump | Set-Content $f
    Fail "$Msg (screen dump: $f)"
}

function Tap-Ui([string]$Attr, [string]$Value, [int]$TimeoutSec = 30) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        $pt = Find-UiCenter (Get-UiDump) $Attr $Value
        if ($pt) {
            Adb shell input tap $pt.X $pt.Y | Out-Null
            return $true
        }
        Start-Sleep -Seconds 2
    }
    $false
}

# ---------------------------------------------------------- desktop side ------

Start-SoakDaemon
$status = Invoke-Cli status

if ($FreshNetwork -and $status.Out -notmatch 'no network on this device') {
    Step 'Dissolving the old soak network'
    $r = Invoke-Cli delete
    if (-not $r.Ok) { Warn "delete failed ($($r.Out.Trim())) — continuing" }
    Start-Sleep -Seconds 2
    $status = Invoke-Cli status
}
if ($status.Out -match 'no network on this device') {
    Step 'Creating network "soaknet"'
    $r = Invoke-Cli create soaknet
    if (-not $r.Ok) { Fail "create failed: $($r.Out)" }
}

function Save-SoakConfig {
    $appUid = Get-AppUid
    AdbSoft shell settings put secure always_on_vpn_app $script:Pkg | Out-Null
    $aov = (AdbSoft shell settings get secure always_on_vpn_app).Trim()
    [ordered]@{
        createdAt      = (Get-Date).ToString('o')
        appUid         = $appUid
        adbSerial      = (Resolve-AdbSerial)
        alwaysOnVpnSet = ($aov -eq $script:Pkg)
    } | ConvertTo-Json | Set-Content $script:ConfigPath
    Step "Config saved (uid $appUid, always-on VPN: $aov). Ready for run-soak.ps1."
}

if ((Get-PhoneState).Online) {
    Step 'Emulator already online in soaknet — nothing to do'
    Save-SoakConfig
    exit 0
}

$r = Invoke-Cli ticket
if (-not $r.Ok) { Fail "ticket failed: $($r.Out)" }
$ticket = ($r.Out -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 1).Trim()
$ticket | Set-Content (Join-Path $script:SoakRoot 'ticket.txt')

# Auto-approver: watch for the join request, approve without ceremony. The SAS
# emoji exist to defeat a human-in-the-middle; a single-host throwaway test
# network has no human and no middle.
$watchOut = Join-Path $script:SoakRoot 'setup-watch.txt'
Remove-Item $watchOut -Force -Confirm:$false -ErrorAction SilentlyContinue
$watch = Start-Process -FilePath (Get-SoakBin 'nullgate-cli.exe') `
    -ArgumentList '--socket', $script:SoakSock, 'watch' `
    -RedirectStandardOutput $watchOut -PassThru -WindowStyle Hidden

try {
    # ------------------------------------------------------ phone side --------
    Step 'Preparing the app (clear data, pre-grant consents)'
    if ($FreshNetwork) { AdbSoft shell pm clear $script:Pkg | Out-Null }
    # pm clear resets appops and runtime permissions, so the grants come after.
    # ACTIVATE_VPN removes the VPN consent dialog (adb may set appops on the
    # emulator); POST_NOTIFICATIONS removes the permission dialog MainActivity
    # requests on first launch — a system dialog that sits ON TOP of the app,
    # which is invisible in a uiautomator search for our own widgets.
    AdbSoft shell appops set $script:Pkg ACTIVATE_VPN allow | Out-Null
    AdbSoft shell pm grant $script:Pkg android.permission.POST_NOTIFICATIONS | Out-Null

    Step 'Driving the join UI'
    Wake-Screen
    Adb shell am start -n "$script:Pkg/.MainActivity" | Out-Null
    Start-Sleep -Seconds 5
    # Sweep any straggler system dialog (permission prompts have an "Allow" button).
    $dump = Get-UiDump
    foreach ($btn in 'Allow', 'While using the app', 'OK') {
        $pt = Find-UiCenter $dump 'text' $btn
        if ($pt) { Adb shell input tap $pt.X $pt.Y | Out-Null; Start-Sleep -Seconds 2; break }
    }
    if (-not (Tap-Ui 'text' 'Join with a ticket')) { Fail-WithUiDump 'never saw the "Join with a ticket" button' }
    Start-Sleep -Seconds 2
    if (-not (Tap-Ui 'class' 'android.widget.EditText')) { Fail-WithUiDump 'never saw the ticket field' }
    Start-Sleep -Seconds 1
    Adb shell input text $ticket | Out-Null
    Start-Sleep -Seconds 1
    if (-not (Tap-Ui 'text' 'Join')) { Fail-WithUiDump 'never saw the Join button' }

    Step 'Waiting for the join request (auto-approving)…'
    $approved = $false
    $deadline = (Get-Date).AddMinutes(10)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        if (-not $approved -and (Test-Path $watchOut)) {
            $m = Select-String -Path $watchOut -Pattern 'approve:\s+nullgate-cli approve (\S+)' |
                Select-Object -First 1
            if ($m) {
                $nodeId = $m.Matches[0].Groups[1].Value
                Step "Approving join from $($nodeId.Substring(0, 16))…"
                $ar = Invoke-Cli approve $nodeId
                if (-not $ar.Ok) { Fail "approve failed: $($ar.Out)" }
                $approved = $true
            }
        }
        if ($approved -and (Get-PhoneState).Online) { break }
    }
    if (-not (Get-PhoneState).Online) {
        Fail 'emulator never came online. Check the app screen; evidence in the soak daemon log.'
    }
    Step 'Emulator joined soaknet and is ONLINE'
} finally {
    try { Stop-Process -Id $watch.Id -Force -Confirm:$false -ErrorAction Stop } catch {}
}

# ------------------------------------------------------ persist run config ----
Save-SoakConfig
