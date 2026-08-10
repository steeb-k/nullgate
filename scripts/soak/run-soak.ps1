<#
.SYNOPSIS
  Run one long-running Android soak scenario against the soaknet emulator.
  Requires scripts\soak\setup-soak.ps1 to have been run once.

  Every run writes target\soak\runs\<stamp>-<scenario>\ containing events.jsonl
  (every state change + measurement), raw netstats snapshots, a logcat capture,
  and summary.json with the verdict. Exit code 0 = pass, 1 = fail, so runs can
  be chained from a wrapper or CI-of-one overnight script.

.PARAMETER Scenario
  idle        - T1: phone idle+screen-off; hourly bytes/PSS/reachability samples.
  doze        - T2: force-doze N cycles; measure recovery time after each wake.
  flap        - T3: random wifi/cell/airplane transitions; recovery after each.
  blackhole   - T4 (the killer test): silently firewall the emulator at the HOST
                so Android sees no network change; measure unassisted recovery.
                Needs an elevated shell. -WithDoze adds force-idle to the block
                window for the closest reproduction of the real phone bug.
  kill        - T6: crash the app process; measure the system restart path.
  attribution - T7: half the run wifi-only, half cellular-only; bytes must land
                in the matching netstats bucket (catches wifi billed as mobile).
  leak        - T8: stop the desktop peer so the phone dials an unreachable
                member; hourly PSS + bytes (iroh#4293 growth on Android).

.PARAMETER Hours               Duration for idle/flap/attribution/leak.
.PARAMETER Cycles              Cycle count for doze/blackhole/kill.
.PARAMETER HoldMinutes         Doze/blackhole hold time per cycle.
.PARAMETER RecoveryTimeoutSec  How long a cycle may take to recover before it
                               counts as failed (default 600 = one background
                               tick + margin; drop to 60 post-fix).
.PARAMETER MaxIdleMbPerHour    If > 0, idle/leak fail when the phone exceeds
                               this. 0 (default) = record-only baseline mode.
.EXAMPLE
  pwsh -File scripts\soak\run-soak.ps1 -Scenario blackhole -Cycles 12 -HoldMinutes 20 -WithDoze
  pwsh -File scripts\soak\run-soak.ps1 -Scenario idle -Hours 24
#>
param(
    [Parameter(Mandatory)]
    [ValidateSet('idle', 'doze', 'flap', 'blackhole', 'kill', 'attribution', 'leak')]
    [string]$Scenario,
    [double]$Hours = 24,
    [int]$Cycles = 12,
    [int]$HoldMinutes = 30,
    [int]$RecoveryTimeoutSec = 600,
    [double]$MaxIdleMbPerHour = 0,
    [switch]$WithDoze
)

. (Join-Path $PSScriptRoot 'soaklib.ps1')

$config = Read-SoakConfig
$appUid = [int]$config.appUid
$runDir = New-SoakRun $Scenario
Step "Run dir: $runDir"

Start-SoakDaemon
if (-not (Get-PhoneState).DaemonUp) { Fail 'soak daemon unreachable' }

# Results accumulated by the scenario, folded into summary.json at the end.
$recoveries = [Collections.Generic.List[double]]::new()
$failures = 0
$cyclesRun = 0
$notes = [Collections.Generic.List[string]]::new()

$logcat = Start-LogcatCapture
$startSnap = Save-NetstatsSnapshot -TargetUid $appUid -Label 'start'
$startTime = Get-Date
Write-SoakEvent 'run-start' @{ uid = $appUid; params = "$Hours h / $Cycles cyc / $HoldMinutes min hold" }

# One recovery measurement: wait for offline-then-online around a disruption.
# Returns $true on success and records the time; $false counts a failure.
function Measure-Recovery([string]$What) {
    $t = Wait-PhoneState -Online $true -TimeoutSec $RecoveryTimeoutSec
    if ($t -lt 0) {
        $script:failures++
        Write-SoakEvent 'recovery-FAILED' @{ after = $What; timeoutSec = $RecoveryTimeoutSec }
        $false
    } else {
        $script:recoveries.Add($t)
        Write-SoakEvent 'recovered' @{ after = $What; seconds = $t }
        $true
    }
}

function Sample-Vitals([string]$Label) {
    $st = Get-PhoneState
    $pss = Get-AppPssMb
    $snap = Save-NetstatsSnapshot -TargetUid $appUid -Label $Label
    $delta = Get-NetstatsDeltaMb $script:startSnap $snap
    Write-SoakEvent 'sample' @{
        label = $Label; online = $st.Online; direct = $st.Direct; pssMb = $pss
        totalMb = ($delta.ALL.rxMb + $delta.ALL.txMb)
        wifiMb = $(if ($delta.ContainsKey('WIFI')) { $delta.WIFI.rxMb + $delta.WIFI.txMb } else { 0 })
        mobileMb = $(if ($delta.ContainsKey('MOBILE')) { $delta.MOBILE.rxMb + $delta.MOBILE.txMb } else { 0 })
    }
    $snap
}

try {
    switch ($Scenario) {

        # ------------------------------------------------------------- T1 -----
        'idle' {
            # A phone on the nightstand: screen off, on battery, dozing as
            # Android sees fit. Hourly samples; reachability checked each hour.
            Sleep-Screen
            Adb shell dumpsys battery unplug | Out-Null
            AdbSoft shell dumpsys deviceidle enable | Out-Null
            $end = $startTime.AddHours($Hours)
            $hour = 0
            while ((Get-Date) -lt $end) {
                Start-Sleep -Seconds 3600
                $hour++
                $cyclesRun++
                Sample-Vitals "hour-$hour" | Out-Null
                if (-not (Get-PhoneState).Online) {
                    # An idle phone with intact networking must stay reachable.
                    Wake-Screen; Sleep-Screen   # nudge, then judge the recovery
                    if (-not (Measure-Recovery "idle-hour-$hour-drop")) { }
                }
            }
        }

        # ------------------------------------------------------------- T2 -----
        'doze' {
            for ($c = 1; $c -le $Cycles; $c++) {
                $cyclesRun++
                Write-SoakEvent 'cycle-start' @{ cycle = $c; of = $Cycles }
                Sleep-Screen
                Enter-Doze
                Start-Sleep -Seconds ($HoldMinutes * 60)
                $during = Get-PhoneState
                Write-SoakEvent 'doze-held' @{ cycle = $c; onlineDuringDoze = $during.Online }
                Exit-Doze
                Wake-Screen
                Measure-Recovery "doze-cycle-$c" | Out-Null
                Sample-Vitals "doze-$c" | Out-Null
                Start-Sleep -Seconds 60   # settle between cycles
            }
        }

        # ------------------------------------------------------------- T3 -----
        'flap' {
            # Random walk over the transitions a commuting phone actually makes.
            # Each transition must be survived; the transition list is logged so
            # any failure is reproducible.
            $rng = [Random]::new()
            $end = $startTime.AddHours($Hours)
            while ((Get-Date) -lt $end) {
                $cyclesRun++
                $kind = @('wifi-to-cell', 'cell-to-wifi', 'airplane-blip', 'both-off-on')[$rng.Next(4)]
                Write-SoakEvent 'flap' @{ n = $cyclesRun; kind = $kind }
                switch ($kind) {
                    'wifi-to-cell' { Set-Mobile $true; Set-Wifi $false }
                    'cell-to-wifi' { Set-Wifi $true; Start-Sleep -Seconds 10; Set-Mobile $false }
                    'airplane-blip' { Set-Airplane $true; Start-Sleep -Seconds 30; Set-Airplane $false }
                    'both-off-on' { Set-Wifi $false; Set-Mobile $false; Start-Sleep -Seconds 60; Restore-Network }
                }
                Measure-Recovery $kind | Out-Null
                if ($cyclesRun % 10 -eq 0) { Sample-Vitals "flap-$cyclesRun" | Out-Null }
                Start-Sleep -Seconds ($rng.Next(300, 1200))   # 5-20 min between flaps
                Restore-Network
                Start-Sleep -Seconds 20
            }
        }

        # ------------------------------------------------------------- T4 -----
        'blackhole' {
            # The scenario the phone actually fails: connectivity dies with NO
            # observable network change inside the guest. Today's build is
            # expected to fail this until the health-check/rebind work lands.
            for ($c = 1; $c -le $Cycles; $c++) {
                # Fresh guest NAT every 3 cycles (see Restart-Emulator) — and each
                # reboot is itself a test of the always-on boot-recovery path.
                if ($c -gt 1 -and (($c - 1) % 3) -eq 0) {
                    Write-SoakEvent 'emulator-reboot' @{ beforeCycle = $c }
                    Restart-Emulator
                    $t = Wait-PhoneState -Online $true -TimeoutSec 300
                    Write-SoakEvent 'post-reboot-online' @{ seconds = $t }
                    if ($t -lt 0) { $failures++; $notes.Add("cycle ${c}: not online after emulator reboot") }
                }
                $cyclesRun++
                Write-SoakEvent 'cycle-start' @{ cycle = $c; of = $Cycles; withDoze = [bool]$WithDoze }
                if ($WithDoze) { Sleep-Screen; Enter-Doze }
                Enable-Blackhole
                # How long until the DESKTOP notices the phone is gone is itself
                # a useful number (connection-derived presence lag).
                $gone = Wait-PhoneState -Online $false -TimeoutSec 300
                Write-SoakEvent 'blackhole-on' @{ cycle = $c; desktopNoticedAfterSec = $gone }
                Start-Sleep -Seconds ($HoldMinutes * 60)
                Disable-Blackhole
                if ($WithDoze) { Exit-Doze }   # deliberately NO screen wake: recovery must be unassisted
                # Environment integrity: a failed cycle only counts against the APP
                # if doze is genuinely lifted and the guest can genuinely reach the
                # network (two prior marathons were invalidated by exactly these).
                $envDoze = (AdbSoft shell 'dumpsys deviceidle get deep').Trim()
                $envPing = ((AdbSoft shell 'ping -c 1 -W 3 8.8.8.8 >/dev/null 2>&1 && echo ok').Trim() -eq 'ok')
                Write-SoakEvent 'env-check' @{ cycle = $c; doze = $envDoze; guestPing = $envPing }
                Write-SoakEvent 'blackhole-off' @{ cycle = $c }
                Measure-Recovery "blackhole-cycle-$c" | Out-Null
                Sample-Vitals "blackhole-$c" | Out-Null
                Start-Sleep -Seconds 120
            }
        }

        # ------------------------------------------------------------- T6 -----
        'kill' {
            for ($c = 1; $c -le $Cycles; $c++) {
                $cyclesRun++
                $before = Get-AppPid
                if (-not $before) { $notes.Add("cycle ${c}: app not running before kill"); }
                Sleep-Screen
                AdbSoft shell am crash $script:Pkg | Out-Null
                Start-Sleep -Seconds 5
                $after = Get-AppPid
                Write-SoakEvent 'killed' @{ cycle = $c; pidBefore = $before; pidAfter = $after }
                Measure-Recovery "kill-cycle-$c" | Out-Null
                Sample-Vitals "kill-$c" | Out-Null
                Start-Sleep -Seconds 60
            }
        }

        # ------------------------------------------------------------- T7 -----
        'attribution' {
            # Phase A: wifi only. Phase B: cellular only. Bytes recorded in the
            # bucket for a radio that is OFF are mis-attribution — the prime
            # suspect for the 28GB "mobile data" reading.
            $half = [math]::Max($Hours / 2, 0.5)
            Set-Wifi $true; Set-Mobile $false
            Start-Sleep -Seconds 30
            $a0 = Save-NetstatsSnapshot -TargetUid $appUid -Label 'phaseA-start'
            Start-Sleep -Seconds ($half * 3600)
            $a1 = Save-NetstatsSnapshot -TargetUid $appUid -Label 'phaseA-end'
            $dA = Get-NetstatsDeltaMb $a0 $a1
            $mobileWhileWifiOnly = $(if ($dA.ContainsKey('MOBILE')) { $dA.MOBILE.rxMb + $dA.MOBILE.txMb } else { 0 })
            Write-SoakEvent 'phaseA-wifi-only' @{ mobileMb = $mobileWhileWifiOnly; wifiMb = $(if ($dA.ContainsKey('WIFI')) { $dA.WIFI.rxMb + $dA.WIFI.txMb } else { 0 }) }

            Set-Mobile $true; Set-Wifi $false
            Start-Sleep -Seconds 30
            $b0 = Save-NetstatsSnapshot -TargetUid $appUid -Label 'phaseB-start'
            Start-Sleep -Seconds ($half * 3600)
            $b1 = Save-NetstatsSnapshot -TargetUid $appUid -Label 'phaseB-end'
            $dB = Get-NetstatsDeltaMb $b0 $b1
            $wifiWhileCellOnly = $(if ($dB.ContainsKey('WIFI')) { $dB.WIFI.rxMb + $dB.WIFI.txMb } else { 0 })
            Write-SoakEvent 'phaseB-cell-only' @{ wifiMb = $wifiWhileCellOnly; mobileMb = $(if ($dB.ContainsKey('MOBILE')) { $dB.MOBILE.rxMb + $dB.MOBILE.txMb } else { 0 }) }

            $cyclesRun = 2
            if ($mobileWhileWifiOnly -gt 1) { $failures++; $notes.Add("phase A: $mobileWhileWifiOnly MB landed in MOBILE with cellular off") }
            if ($wifiWhileCellOnly -gt 1) { $failures++; $notes.Add("phase B: $wifiWhileCellOnly MB landed in WIFI with wifi off") }
        }

        # ------------------------------------------------------------- T8 -----
        'leak' {
            # Make the one peer unreachable (stop the desktop daemon) and watch
            # the phone's month in fast-forward: dial-forever, address-map
            # growth, per-attempt cost creep. PSS + bytes sampled hourly.
            Step 'Stopping soak daemon — phone now has one permanently unreachable member'
            Stop-SoakDaemon
            $end = $startTime.AddHours($Hours)
            $hour = 0
            $pssSeries = [Collections.Generic.List[double]]::new()
            while ((Get-Date) -lt $end) {
                Start-Sleep -Seconds 3600
                $hour++; $cyclesRun++
                $pss = Get-AppPssMb
                if ($null -ne $pss) { $pssSeries.Add($pss) }
                $snap = Save-NetstatsSnapshot -TargetUid $appUid -Label "leak-hour-$hour"
                $delta = Get-NetstatsDeltaMb $startSnap $snap
                Write-SoakEvent 'sample' @{ label = "leak-hour-$hour"; pssMb = $pss; totalMb = ($delta.ALL.rxMb + $delta.ALL.txMb) }
            }
            if ($pssSeries.Count -ge 3) {
                $growth = $pssSeries[-1] - $pssSeries[0]
                $notes.Add("PSS $($pssSeries[0]) MB -> $($pssSeries[-1]) MB over $hour h (growth $growth MB)")
            }
            Step 'Restarting soak daemon'
            Start-SoakDaemon
            Measure-Recovery 'leak-daemon-restored' | Out-Null
        }
    }
} finally {
    # Leave the emulator and host the way we found them, whatever happened.
    Disable-Blackhole
    Exit-Doze
    Restore-Network
    Stop-LogcatCapture
}

# ------------------------------------------------------------- summary --------
$endSnap = Save-NetstatsSnapshot -TargetUid $appUid -Label 'end'
$total = Get-NetstatsDeltaMb $startSnap $endSnap
$elapsedH = [math]::Max(((Get-Date) - $startTime).TotalHours, 0.01)
$totalMb = [math]::Round($total.ALL.rxMb + $total.ALL.txMb, 1)
$mbPerHour = [math]::Round($totalMb / $elapsedH, 2)

$dataFail = ($MaxIdleMbPerHour -gt 0 -and $Scenario -in @('idle', 'leak') -and $mbPerHour -gt $MaxIdleMbPerHour)
if ($dataFail) { $notes.Add("data rate $mbPerHour MB/h exceeds limit $MaxIdleMbPerHour MB/h") }
$pass = ($failures -eq 0 -and -not $dataFail -and $cyclesRun -gt 0)

$summary = [ordered]@{
    scenario     = $Scenario
    pass         = $pass
    cycles       = $cyclesRun
    failures     = $failures
    elapsedHours = [math]::Round($elapsedH, 2)
    dataMbTotal  = $totalMb
    dataMbPerHr  = $mbPerHour
    recoveryP50  = Get-Percentile $recoveries.ToArray() 0.5
    recoveryP95  = Get-Percentile $recoveries.ToArray() 0.95
    recoveryMax  = $(if ($recoveries.Count) { [math]::Round(($recoveries | Measure-Object -Maximum).Maximum, 1) } else { $null })
    notes        = $notes.ToArray()
}
$summary | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $runDir 'summary.json')
Write-SoakEvent 'run-end' @{ pass = $pass; failures = $failures; dataMbPerHr = $mbPerHour }

Write-Host ''
Write-Host ('=' * 60)
Write-Host ("SOAK {0}: {1}" -f $Scenario.ToUpper(), $(if ($pass) { 'PASS' } else { 'FAIL' })) -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' })
$summary.GetEnumerator() | ForEach-Object { Write-Host ("  {0,-14} {1}" -f $_.Key, ($_.Value -join '; ')) }
Write-Host ('=' * 60)
exit $(if ($pass) { 0 } else { 1 })
