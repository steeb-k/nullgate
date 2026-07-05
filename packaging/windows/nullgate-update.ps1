<#
.SYNOPSIS
  Nullgate (Nullgate) Windows install-or-update engine + scheduled-task
  helper.

  The Windows analog of packaging/linux/nullgate-update. Compares the installed daemon
  version to the latest release of the PUBLIC steeb-k/nullgate repo
  and, if newer, downloads the MSI and applies it silently. The MSI's MajorUpgrade
  handling does the heavy lifting (stop service -> replace files -> restart
  service), so "apply" is just `msiexec /i <msi> /qn`.

.PARAMETER Check
  Report whether an update is available; make no changes.

.PARAMETER RegisterTask
  Register the daily "NullgateUpdate" scheduled task (runs this script as SYSTEM).
  Invoked by the MSI on install.

.PARAMETER UnregisterTask
  Remove the "NullgateUpdate" scheduled task.

.NOTES
  Public repo => no auth. Version is the source of truth (compared to the release
  tag). Lives in "Program Files\Nullgate\bin"; it locates nullgate-daemon.exe next to itself
  via $PSScriptRoot.
#>
[CmdletBinding(DefaultParameterSetName = 'Update')]
param(
    [Parameter(ParameterSetName = 'Update')]   [switch]$Check,
    [Parameter(ParameterSetName = 'Register')] [switch]$RegisterTask,
    [Parameter(ParameterSetName = 'Unregister')][switch]$UnregisterTask
)

$ErrorActionPreference = 'Stop'

$Repo      = if ($env:NULLGATE_BINARIES_REPO) { $env:NULLGATE_BINARIES_REPO } else { 'steeb-k/nullgate' }
$AssetGlob = '*windows-x86_64.msi'
$TaskName  = 'NullgateUpdate'
$BinDir    = $PSScriptRoot
$DaemonExe = Join-Path $BinDir 'nullgate-daemon.exe'
$GuiExe    = Join-Path $BinDir 'nullgate.exe'
$ScriptPath = $PSCommandPath

# Log to the machine-wide data dir (where the LocalSystem daemon also writes).
$DataDir = Join-Path $env:ProgramData 'nullgate'
$LogFile = Join-Path $DataDir 'update.log'

function Write-Log {
    param([string]$Message)
    $line = "{0}  {1}" -f (Get-Date -Format 's'), $Message
    Write-Host "nullgate-update: $Message"
    try {
        if (-not (Test-Path $DataDir)) { New-Item -ItemType Directory -Force -Path $DataDir | Out-Null }
        Add-Content -Path $LogFile -Value $line -Encoding UTF8
    } catch { }
}

# -- Scheduled-task management ------------------------------------------------
function Register-UpdateTask {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument ("-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"{0}`"" -f $ScriptPath)

    # Daily (with a random spread) plus a delayed run shortly after each boot —
    # mirrors the Linux timer's "after login + daily, randomized".
    $daily = New-ScheduledTaskTrigger -Daily -At 3am
    $daily.RandomDelay = 'PT2H'
    $boot = New-ScheduledTaskTrigger -AtStartup
    $boot.Delay = 'PT5M'

    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings  = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -DontStopOnIdleEnd -ExecutionTimeLimit (New-TimeSpan -Hours 1)

    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger @($daily, $boot) `
        -Principal $principal -Settings $settings `
        -Description 'Nullgate daily auto-update check' -Force | Out-Null
    Write-Log "registered scheduled task '$TaskName'"
}

function Unregister-UpdateTask {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Log "removed scheduled task '$TaskName'"
    }
}

# -- Update logic ------------------------------------------------------------
function Get-InstalledVersion {
    if (-not (Test-Path $DaemonExe)) { return $null }
    $out = & $DaemonExe --version 2>$null
    if ($out -match '(\d+\.\d+\.\d+)') { return $matches[1] }
    return $null
}

# -- GUI restart across the SYSTEM/user session boundary ---------------------
# This task runs as SYSTEM (session 0); the tray GUI runs as the logged-in user
# in their interactive session. The MSI's Restart Manager can close the GUI but
# can't relaunch it back into the user session, so the GUI would be left dead (or
# stale) after an update. We handle it ourselves: stop the GUI before the MSI (so
# nullgate.exe swaps in place with no pending reboot), then relaunch it in the
# user's session afterwards.
function Get-ConsoleUser {
    # 'DOMAIN\user' of the interactive (console) session, or $null if nobody's on.
    try {
        $u = (Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop).UserName
        if ($u) { return $u }
    } catch { }
    return $null
}

function Stop-Gui {
    # Close any running nullgate.exe (the tray agent and/or the GUI window — both are
    # nullgate.exe) so the MSI can replace it in place. Returns $true if one was
    # running, so we know to relaunch the tray agent afterwards.
    $procs = Get-Process -Name 'nullgate' -ErrorAction SilentlyContinue
    if (-not $procs) { return $false }
    Write-Log "closing the running tray agent/GUI before applying the update"
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800   # let file handles release
    return $true
}

function Restart-Gui {
    # Relaunch the tray agent (`--agent`) in $User's interactive session, via a
    # transient scheduled task with a Limited (non-elevated) token — it must run
    # unprivileged, exactly as it normally does. The agent restores the tray; the
    # user reopens the GUI window on demand. Best-effort; logged on failure.
    param([string]$User)
    if (-not $User) { Write-Log "no interactive user; the tray agent will start at next login"; return }
    if (-not (Test-Path $GuiExe)) { Write-Log "nullgate.exe not found at $GuiExe; skipping relaunch"; return }
    $task = 'NullgateGuiRelaunch'
    try {
        $action    = New-ScheduledTaskAction -Execute $GuiExe -Argument '--agent'
        $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
        $settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $task
        Write-Log "relaunched the tray agent as $User (--agent)"
        Start-Sleep -Seconds 3   # give the task time to fire before we remove it
    } catch {
        Write-Log "tray agent relaunch failed: $($_.Exception.Message)"
    } finally {
        Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    }
}

function Invoke-Update {
    param([switch]$CheckOnly)

    try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

    $installed = Get-InstalledVersion
    if (-not $installed) {
        Write-Log "nullgate-daemon.exe not found next to the updater; nothing to do"
        return
    }
    Write-Log "checking $Repo for a newer release (installed: $installed)"

    $headers = @{ 'User-Agent' = 'nullgate-update'; 'Accept' = 'application/vnd.github+json' }
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    $tag = $rel.tag_name
    if (-not $tag) { Write-Log "could not determine the latest release tag"; return }
    $latest = $tag.TrimStart('v')

    if ([version]$latest -le [version]$installed) {
        Write-Log "up to date (latest: $latest)"
        return
    }
    Write-Log "update available: $installed -> $latest"
    if ($CheckOnly) { return }

    $asset = $rel.assets | Where-Object { $_.name -like $AssetGlob } | Select-Object -First 1
    if (-not $asset) { Write-Log "release $tag has no $AssetGlob asset"; return }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("nullgate-{0}.msi" -f $latest)
    Write-Log "downloading $($asset.name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmp -UseBasicParsing -Headers @{ 'User-Agent' = 'nullgate-update' }

    # Close the tray GUI first (see the session-boundary note above), remembering
    # whether it was running and who to relaunch it as.
    $guiUser       = Get-ConsoleUser
    $guiWasRunning = Stop-Gui

    $msiLog = Join-Path $DataDir 'update-msi.log'
    Write-Log "applying $tmp (msiexec /qn)"
    $p = Start-Process -FilePath 'msiexec.exe' `
        -ArgumentList @('/i', "`"$tmp`"", '/qn', '/norestart', '/l*v', "`"$msiLog`"") `
        -Wait -PassThru
    # 0 = success, 3010 = success, reboot required.
    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) {
        Write-Log "updated to $latest (msiexec exit $($p.ExitCode))"
    } else {
        Write-Log "msiexec failed (exit $($p.ExitCode)); see $msiLog"
    }
    # Bring the tray GUI back — the new version on success, or the existing one if
    # the update failed — so the user is never left without it.
    if ($guiWasRunning) { Restart-Gui -User $guiUser }
    Remove-Item $tmp -ErrorAction SilentlyContinue
}

# -- Entry point -------------------------------------------------------------
try {
    switch ($PSCmdlet.ParameterSetName) {
        'Register'   { Register-UpdateTask }
        'Unregister' { Unregister-UpdateTask }
        default      { Invoke-Update -CheckOnly:$Check }
    }
} catch {
    Write-Log "error: $($_.Exception.Message)"
    exit 1
}
