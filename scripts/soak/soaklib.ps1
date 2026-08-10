# Shared library for the Android soak-test harness. Dot-sourced by
# setup-soak.ps1 and run-soak.ps1 — not meant to be run on its own.
#
# Design constraints (see scripts/soak/README.md):
#  - Zero app changes: phone reachability is judged from the DESKTOP side
#    (a throwaway "soaknet" daemon + nullgate-cli), data usage from per-UID
#    netstats on the emulator. Raw dumps are always saved so numbers can be
#    re-parsed later if a dumpsys format shifts.
#  - Total isolation from the real Nullgate install: own --socket, own
#    NULLGATE_DATA_DIR, TUN disabled. Nothing here touches ProgramData.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:RepoRoot   = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$script:SoakRoot   = Join-Path $script:RepoRoot 'target\soak'
$script:SoakSock   = Join-Path $script:SoakRoot 'soak.sock'
$script:DaemonData = Join-Path $script:SoakRoot 'daemon-data'
$script:DaemonLogs = Join-Path $script:SoakRoot 'daemon-logs'
$script:DaemonExe  = Join-Path $script:RepoRoot 'target\debug\nullgate-daemon.exe'
$script:CliExe     = Join-Path $script:RepoRoot 'target\debug\nullgate-cli.exe'
$script:ConfigPath = Join-Path $script:SoakRoot 'soak-config.json'
$script:PidPath    = Join-Path $script:SoakRoot 'daemon.pid'
$script:Pkg        = 'io.github.steeb_k.nullgate'
$script:RunDir       = $null    # set by New-SoakRun
# NOT named $script:Scenario: when run-soak.ps1 dot-sources this file, a bare
# `$script:Scenario = ''` assigns to run-soak's own [ValidateSet] PARAMETER of
# that name and throws a validation error before anything runs.
$script:SoakScenario = ''       # set by New-SoakRun
$script:AdbSerial  = $null      # resolved lazily

function Step([string]$m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Warn([string]$m) { Write-Host "WARN: $m" -ForegroundColor Yellow }
function Fail([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

# ------------------------------------------------------------------ adb -------

function Get-AdbExe {
    if (-not $env:ANDROID_HOME) { Fail 'ANDROID_HOME not set (see docs/android-packaging.md).' }
    $adb = Join-Path $env:ANDROID_HOME 'platform-tools\adb.exe'
    if (-not (Test-Path $adb)) { Fail "adb not found at $adb" }
    $adb
}

# Pick the device once per run. A physical phone may be plugged in at the same
# time, so prefer emulators and refuse to guess between two of them.
function Resolve-AdbSerial {
    if ($script:AdbSerial) { return $script:AdbSerial }
    if ($env:ANDROID_SERIAL) { $script:AdbSerial = $env:ANDROID_SERIAL; return $script:AdbSerial }
    $adb = Get-AdbExe
    $lines = @((& $adb devices) | Select-String -Pattern '^(\S+)\s+device$' | ForEach-Object { $_.Matches[0].Groups[1].Value })
    $emus = @($lines | Where-Object { $_ -like 'emulator-*' })
    if ($emus.Count -eq 1) { $script:AdbSerial = $emus[0]; return $script:AdbSerial }
    if ($emus.Count -gt 1) { Fail "Multiple emulators running ($($emus -join ', ')) — set ANDROID_SERIAL." }
    if ($lines.Count -eq 1) { $script:AdbSerial = $lines[0]; return $script:AdbSerial }
    Fail 'No adb device found (boot the emulator first, e.g. scripts\run-android.ps1).'
}

function Adb {
    param([Parameter(ValueFromRemainingArguments)] [string[]]$Rest)
    $adb = Get-AdbExe
    $serial = Resolve-AdbSerial
    $out = & $adb -s $serial @Rest 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "adb $($Rest -join ' ') failed: $out" }
    $out
}

# Same, but failures are logged instead of thrown — for cleanup paths and
# best-effort commands where the run must go on.
function AdbSoft {
    param([Parameter(ValueFromRemainingArguments)] [string[]]$Rest)
    try { Adb @Rest } catch { Warn $_.Exception.Message; '' }
}

function Get-AppUid {
    $out = Adb shell dumpsys package $script:Pkg
    # Older dumps print "userId=10210", newer ones "uid=10210 gids=[…]"; anchor to
    # line start so fields like installerPackageUid=-1 can't match.
    foreach ($p in '(?m)^\s*userId=(\d+)', '(?m)^\s*uid=(\d+)\b') {
        $m = [regex]::Match($out, $p)
        if ($m.Success) { return [int]$m.Groups[1].Value }
    }
    throw "couldn't find the app uid for $script:Pkg — is the app installed?"
}

function Get-AppPid {
    $out = AdbSoft shell pidof $script:Pkg
    $t = $out.Trim()
    if ($t -match '^\d+$') { [int]$t } else { $null }
}

# TOTAL PSS in MB, or $null if the process isn't running.
function Get-AppPssMb {
    $out = AdbSoft shell dumpsys meminfo $script:Pkg -s
    if ($out -match 'TOTAL PSS:\s+(\d+)') { return [math]::Round([int64]$Matches[1] / 1024, 1) }
    $null
}

# ------------------------------------------------------- netstats snapshots ---

# Snapshot per-UID rx/tx bytes, bucketed by interface type (WIFI / MOBILE / …).
# The raw dumpsys output is always written next to the parsed numbers: the
# text format is not a stable API, and the raw file is what lets us re-parse a
# week-long run after fixing the parser instead of re-running it.
function Save-NetstatsSnapshot {
    param([int]$TargetUid, [string]$Label)
    AdbSoft shell cmd netstats force-update | Out-Null   # flush kernel counters; best-effort
    $raw = Adb shell dumpsys netstats detail
    if ($script:RunDir) {
        $safe = ($Label -replace '[^\w\-]', '_')
        $raw | Set-Content -Path (Join-Path $script:RunDir "netstats-$safe.txt")
    }

    # netstats idents print NUMERIC types on modern builds ("type=0, ratType=3"),
    # named ones historically. \b keeps 'type=' from matching inside 'ratType='.
    $typeNames = @{ '0' = 'MOBILE'; '1' = 'WIFI'; '9' = 'ETHERNET'; '17' = 'VPN' }
    $totals = @{}
    $curType = $null; $curUid = -2; $curTag = ''
    foreach ($line in ($raw -split "`r?`n")) {
        if ($line -match 'ident=\[') {
            $curType = if ($line -match '\btype=([A-Za-z0-9_]+)') {
                $t = $Matches[1]
                if ($typeNames.ContainsKey($t)) { $typeNames[$t] }
                elseif ($t -match '^\d+$') { "TYPE$t" }
                else { $t.ToUpper() }
            } else { 'UNKNOWN' }
            $curUid  = if ($line -match 'uid=(-?\d+)') { [int]$Matches[1] } else { -2 }
            $curTag  = if ($line -match 'tag=(0x[0-9a-fA-F]+)') { $Matches[1] } else { '0x0' }
            continue
        }
        # Bucket lines: "st=1691000000 rb=123 rp=3 tb=456 tp=4 ..."
        if ($curUid -eq $TargetUid -and $curTag -eq '0x0' -and $line -match 'rb=(\d+).*?tb=(\d+)') {
            if (-not $totals.ContainsKey($curType)) { $totals[$curType] = @{ rx = [int64]0; tx = [int64]0 } }
            $totals[$curType].rx += [int64]$Matches[1]
            $totals[$curType].tx += [int64]$Matches[2]
        }
    }
    $all = @{ rx = [int64]0; tx = [int64]0 }
    foreach ($v in $totals.Values) { $all.rx += $v.rx; $all.tx += $v.tx }
    $totals['ALL'] = $all
    $totals
}

# Delta of two snapshots in MB, per interface bucket.
function Get-NetstatsDeltaMb {
    param($Before, $After)
    $delta = @{}
    foreach ($k in $After.Keys) {
        $b = if ($Before.ContainsKey($k)) { $Before[$k] } else { @{ rx = [int64]0; tx = [int64]0 } }
        $delta[$k] = @{
            rxMb = [math]::Round(($After[$k].rx - $b.rx) / 1MB, 2)
            txMb = [math]::Round(($After[$k].tx - $b.tx) / 1MB, 2)
        }
    }
    $delta
}

# ----------------------------------------------------- device state levers ----

function Wake-Screen { AdbSoft shell input keyevent KEYCODE_WAKEUP | Out-Null; AdbSoft shell wm dismiss-keyguard | Out-Null }
function Sleep-Screen { AdbSoft shell input keyevent KEYCODE_SLEEP | Out-Null }

# Doze needs the device to believe it's on battery. `force-idle deep` holds
# until `unforce` or a screen-on. Emulator caveat (README): the guest CPU does
# not actually suspend like a phone SoC, so pair with the blackhole for the
# full "silent death" repro.
function Enter-Doze {
    Adb shell dumpsys battery unplug | Out-Null
    AdbSoft shell dumpsys deviceidle enable | Out-Null
    Adb shell dumpsys deviceidle force-idle deep | Out-Null
}
function Exit-Doze {
    AdbSoft shell dumpsys deviceidle unforce | Out-Null
    # `unforce` alone does NOT leave deep idle on this image (verified live
    # 2026-08-09: state stayed IDLE, netd kept the app's network blocked — which
    # silently turned every marathon "recovery window" into continued OS-level
    # network denial and doomed two 12-cycle runs). `disable` exits idle
    # immediately; Enter-Doze re-enables before forcing.
    AdbSoft shell dumpsys deviceidle disable | Out-Null
    AdbSoft shell dumpsys battery reset | Out-Null
}

function Set-Wifi([bool]$On)   { Adb shell svc wifi $(if ($On) { 'enable' } else { 'disable' }) | Out-Null }
function Set-Mobile([bool]$On) { Adb shell svc data $(if ($On) { 'enable' } else { 'disable' }) | Out-Null }
function Set-Airplane([bool]$On) {
    Adb shell cmd connectivity airplane-mode $(if ($On) { 'enable' } else { 'disable' }) | Out-Null
}
function Restore-Network { AdbSoft shell cmd connectivity airplane-mode disable | Out-Null; AdbSoft shell svc wifi enable | Out-Null; AdbSoft shell svc data enable | Out-Null }

# --------------------------------------------------------------- blackhole ----

# The key trick of the whole harness: silently kill the guest's connectivity so
# that inside Android the network looks perfectly healthy (no ConnectivityManager
# callback fires) while every iroh path is dead — exactly what a doze/NAT-timeout
# does to the phone. Implemented as root iptables DROPs INSIDE the guest
# (`adb root` works on AOSP images): outbound UDP (QUIC, DNS) and TCP to
# 80/443/8443 (relay, pkarr, probes). DROP sends no errors — silent by design.
#
# History: v1 blocked the qemu process at the Windows firewall. That does NOT
# come back when the rules are removed — qemu's slirp NAT wedges permanently
# once its host sockets error, and the guest transmits nothing until the
# emulator restarts (both 2026-08-07/08 blackhole runs: tx=0B for the whole
# night after cycle 1, 7 perfect in-guest node rebuilds sending into a dead
# NAT). Guest-side iptables round-trips cleanly and needs no elevated shell.
$script:BlackholeRules = @(
    '-p udp -j DROP',
    '-p tcp --dport 443 -j DROP',
    '-p tcp --dport 80 -j DROP',
    '-p tcp --dport 8443 -j DROP'
)

function Enable-Blackhole {
    Adb root | Out-Null
    AdbSoft wait-for-device | Out-Null
    Start-Sleep -Seconds 2
    foreach ($r in $script:BlackholeRules) {
        AdbSoft shell "iptables -I OUTPUT $r" | Out-Null
        AdbSoft shell "ip6tables -I OUTPUT $r" | Out-Null
    }
}

function Disable-Blackhole {
    foreach ($r in $script:BlackholeRules) {
        AdbSoft shell "iptables -D OUTPUT $r" | Out-Null
        AdbSoft shell "ip6tables -D OUTPUT $r" | Out-Null
    }
}

# ------------------------------------------------------------- soak daemon ----

# The daemon + CLI run from COPIES under target\soak\bin, not target\debug:
# Windows locks a running exe, and a `cargo build` during a multi-day soak run
# must never collide with the live daemon. Copies refresh on daemon start.
function Get-SoakBin([string]$Name) {
    $copy = Join-Path $script:SoakRoot "bin\$Name"
    if (Test-Path $copy) { $copy } else { Join-Path $script:RepoRoot "target\debug\$Name" }
}

function Update-SoakBins {
    $binDir = Join-Path $script:SoakRoot 'bin'
    New-Item -ItemType Directory -Force $binDir | Out-Null
    foreach ($n in 'nullgate-daemon.exe', 'nullgate-cli.exe') {
        $src = Join-Path $script:RepoRoot "target\debug\$n"
        if (-not (Test-Path $src)) { Fail "Build first: cargo build -p ipn-daemon -p ipn-cli (missing $src)" }
        try { Copy-Item $src $binDir -Force } catch { Warn "couldn't refresh $n (in use?) — keeping the existing copy" }
    }
}

function Start-SoakDaemon {
    New-Item -ItemType Directory -Force $script:DaemonData, $script:DaemonLogs | Out-Null
    if (Test-SoakDaemon) { return }
    Update-SoakBins

    $saved = @{}
    $envs = @{
        NULLGATE_DATA_DIR             = $script:DaemonData
        NULLGATE_LOG_DIR              = $script:DaemonLogs
        NULLGATE_DISABLE_TUN          = '1'
        NULLGATE_SECRETS_FILE_ONLY    = '1'
        NULLGATE_DISABLE_POWER_EVENTS = '1'
    }
    foreach ($k in $envs.Keys) { $saved[$k] = [Environment]::GetEnvironmentVariable($k); [Environment]::SetEnvironmentVariable($k, $envs[$k]) }
    try {
        $p = Start-Process -FilePath (Get-SoakBin 'nullgate-daemon.exe') `
            -ArgumentList '--socket', $script:SoakSock, '--data-dir', $script:DaemonData, 'run' `
            -RedirectStandardOutput (Join-Path $script:SoakRoot 'daemon-stdout.log') `
            -RedirectStandardError (Join-Path $script:SoakRoot 'daemon-stderr.log') `
            -PassThru -WindowStyle Hidden
        $p.Id | Set-Content $script:PidPath
    } finally {
        foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
    }

    for ($i = 0; $i -lt 15; $i++) {
        Start-Sleep -Seconds 2
        if (Test-SoakDaemon) { return }
    }
    Fail "soak daemon didn't come up — see $script:SoakRoot\daemon-stderr.log"
}

function Test-SoakDaemon { (Invoke-Cli status).Ok }

function Stop-SoakDaemon {
    if (Test-Path $script:PidPath) {
        $daemonPid = Get-Content $script:PidPath
        try { Stop-Process -Id $daemonPid -Force -Confirm:$false -ErrorAction Stop } catch {}
        Remove-Item $script:PidPath -Force -Confirm:$false -ErrorAction SilentlyContinue
    }
}

function Invoke-Cli {
    param([Parameter(ValueFromRemainingArguments)] [string[]]$Rest)
    $cli = Get-SoakBin 'nullgate-cli.exe'
    if (-not (Test-Path $cli)) { Fail "Build first: cargo build -p ipn-daemon -p ipn-cli (missing $cli)" }
    $out = & $cli --socket $script:SoakSock @Rest 2>&1 | Out-String
    @{ Ok = ($LASTEXITCODE -eq 0); Out = $out }
}

# The soaknet has exactly two members: this daemon (originator) and the
# emulator. "Is the phone online" = does the daemon hold a live connection to
# its one non-self member. That is ground truth for reachability-of-phone.
# A dead daemon (it's a debug build; a noq overflow panic killed one mid-run on
# 2026-08-07) is restarted here so a multi-day run measures the PHONE, not a
# dead observer; the restart is logged as an event when a run is active.
function Get-PhoneState {
    $r = Invoke-Cli status
    if (-not $r.Ok) {
        if ($script:RunDir) { Write-SoakEvent 'daemon-restart' @{ reason = 'status unreachable' } }
        try { Start-SoakDaemon } catch { return @{ DaemonUp = $false; Online = $false; Direct = $null } }
        $r = Invoke-Cli status
    }
    if (-not $r.Ok) { return @{ DaemonUp = $false; Online = $false; Direct = $null } }
    foreach ($line in ($r.Out -split "`r?`n")) {
        if ($line -match '^\s+\[(online |offline)\]') {
            return @{
                DaemonUp = $true
                Online   = ($Matches[1] -eq 'online ')
                Direct   = if ($line -match '\(direct\)') { $true } elseif ($line -match '\(relay\)') { $false } else { $null }
            }
        }
    }
    @{ DaemonUp = $true; Online = $false; Direct = $null }
}

# Poll until the phone reaches $Online. Returns elapsed seconds, or -1 on
# timeout. 5 s polling bounds the measurement error; scenarios treat < 0 as a
# failed cycle, never as an excuse to stop the run.
function Wait-PhoneState {
    param([bool]$Online, [int]$TimeoutSec, [int]$PollSec = 5)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        if ((Get-PhoneState).Online -eq $Online) { return [math]::Round($sw.Elapsed.TotalSeconds) }
        Start-Sleep -Seconds $PollSec
    }
    -1
}

# ------------------------------------------------------------ run + logging ---

function New-SoakRun([string]$Scenario) {
    $script:SoakScenario = $Scenario
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $script:RunDir = Join-Path $script:SoakRoot "runs\$stamp-$Scenario"
    New-Item -ItemType Directory -Force $script:RunDir | Out-Null
    $script:RunDir
}

function Write-SoakEvent {
    param([string]$Kind, [hashtable]$Data = @{})
    $ev = [ordered]@{ ts = (Get-Date).ToString('o'); scenario = $script:SoakScenario; kind = $Kind }
    foreach ($k in $Data.Keys) { $ev[$k] = $Data[$k] }
    ($ev | ConvertTo-Json -Compress -Depth 6) | Add-Content -Path (Join-Path $script:RunDir 'events.jsonl')
    Write-Host "[$($ev.ts)] $Kind $((@($Data.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" })) -join ' ')"
}

$script:LogcatProc = $null
$script:LogcatSeq = 0

# Capture survives emulator reboots by starting a fresh segment file each time
# (the adb logcat process dies with the device connection).
function Start-LogcatCapture {
    if ($script:LogcatProc) {
        try { Stop-Process -Id $script:LogcatProc.Id -Force -Confirm:$false -ErrorAction Stop } catch {}
    }
    $adb = Get-AdbExe
    $serial = Resolve-AdbSerial
    AdbSoft logcat -c | Out-Null
    $script:LogcatSeq++
    $name = if ($script:LogcatSeq -eq 1) { 'logcat.txt' } else { "logcat-$($script:LogcatSeq).txt" }
    $script:LogcatProc = Start-Process -FilePath $adb -ArgumentList '-s', $serial, 'logcat', '-v', 'epoch', '*:I' `
        -RedirectStandardOutput (Join-Path $script:RunDir $name) `
        -RedirectStandardError (Join-Path $script:RunDir "$name-err.txt") `
        -PassThru -WindowStyle Hidden
    $script:LogcatProc
}

function Stop-LogcatCapture {
    if ($script:LogcatProc) {
        try { Stop-Process -Id $script:LogcatProc.Id -Force -Confirm:$false -ErrorAction Stop } catch {}
        $script:LogcatProc = $null
    }
}

# Reboot the guest: qemu's slirp NAT degrades cumulatively under repeated block
# cycles (by cycle ~4 of the 2026-08-08 run: 20x ping latency, relay TCP dead,
# while every app subsystem still ticked) — a fresh boot keeps the environment
# honest, and each reboot exercises the always-on boot-recovery path for free.
function Restart-Emulator {
    Adb reboot | Out-Null
    $adb = Get-AdbExe
    & $adb wait-for-device 2>&1 | Out-Null
    for ($i = 0; $i -lt 120; $i++) {
        $b = (& $adb -s (Resolve-AdbSerial) shell getprop sys.boot_completed 2>$null | Out-String).Trim()
        if ($b -eq '1') { break }
        Start-Sleep -Seconds 2
    }
    # Root BEFORE starting the logcat capture: `adb root` restarts adbd and kills
    # every adb client, which beheaded run B's captures 21s after each reboot.
    AdbSoft root | Out-Null
    & $adb wait-for-device 2>&1 | Out-Null
    Start-Sleep -Seconds 2
    if ($script:RunDir) { Start-LogcatCapture | Out-Null }
}

function Get-Percentile([double[]]$Values, [double]$P) {
    if (-not $Values -or $Values.Count -eq 0) { return $null }
    $s = $Values | Sort-Object
    $idx = [math]::Min([math]::Ceiling($P * $s.Count) - 1, $s.Count - 1)
    [math]::Round($s[[math]::Max($idx, 0)], 1)
}

function Read-SoakConfig {
    if (-not (Test-Path $script:ConfigPath)) { Fail 'No soak config — run scripts\soak\setup-soak.ps1 first.' }
    Get-Content $script:ConfigPath -Raw | ConvertFrom-Json
}
