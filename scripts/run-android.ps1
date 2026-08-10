<#
.SYNOPSIS
  Build, install, and launch the Nullgate Android app on an emulator or USB device.

  Self-checks the Android dev environment (JDK 17, Android SDK/NDK, cargo-ndk, Rust
  targets, Smart App Control) and refers to docs/android-packaging.md if anything's
  missing. See that doc for the full setup.

.PARAMETER Device     Use a connected physical device instead of the emulator.
.PARAMETER Release    Build the signed release APK (needs android\keystore.properties).
.PARAMETER Avd        Emulator AVD name (default: seed_api35).
.PARAMETER SkipBuild  Install the existing APK without rebuilding.

.EXAMPLE
  pwsh -File scripts\run-android.ps1
  pwsh -File scripts\run-android.ps1 -Device
  pwsh -File scripts\run-android.ps1 -Release
#>
param(
    [switch]$Device,
    [switch]$Release,
    [string]$Avd = "seed_api35",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root   = Split-Path -Parent $PSScriptRoot
$DocRef = "-> See docs/android-packaging.md for setup."
$Pkg    = "io.github.steeb_k.nullgate"
$Act    = "$Pkg/.MainActivity"

function Fail($m) { Write-Host "ERROR: $m" -ForegroundColor Red; Write-Host $DocRef -ForegroundColor Yellow; exit 1 }
function Warn($m) { Write-Host "WARN:  $m" -ForegroundColor Yellow }
function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }

# ---------------------------------------------------------------- env checks ---
Step "Checking Android dev environment"
$missing = @()
if (-not $env:JAVA_HOME      -or -not (Test-Path "$env:JAVA_HOME\bin\java.exe")) { $missing += "JAVA_HOME (JDK 17)" }
if (-not $env:ANDROID_HOME   -or -not (Test-Path $env:ANDROID_HOME))            { $missing += "ANDROID_HOME (Android SDK)" }
if (-not $env:ANDROID_NDK_HOME -or -not (Test-Path $env:ANDROID_NDK_HOME))      { $missing += "ANDROID_NDK_HOME (NDK r27c)" }
if ($missing.Count) { Fail ("Missing env vars / paths: " + ($missing -join ", ")) }

$adb = Join-Path $env:ANDROID_HOME "platform-tools\adb.exe"
$emu = Join-Path $env:ANDROID_HOME "emulator\emulator.exe"
if (-not (Test-Path $adb)) { Fail "adb not found at $adb (install platform-tools)." }
if (-not (Get-Command cargo-ndk -ErrorAction SilentlyContinue)) { Fail "cargo-ndk not installed (cargo install cargo-ndk)." }

$haveTargets = @()
try { $haveTargets = rustup target list --installed } catch { Fail "rustup not found." }
foreach ($t in @("aarch64-linux-android","armv7-linux-androideabi","x86_64-linux-android")) {
    if ($haveTargets -notcontains $t) { Fail "Rust target '$t' missing (rustup target add $t)." }
}

$sac = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy" -ErrorAction SilentlyContinue).VerifiedAndReputablePolicyState
if ($sac -eq 1) { Fail "Smart App Control is ON — it blocks cargo/cargo-ndk build scripts (os error 4551). Turn it Off." }

if ($Release -and -not (Test-Path (Join-Path $root "android\keystore.properties"))) {
    Fail "Release build needs android\keystore.properties (your signing key). Debug build (drop -Release) needs no key."
}

# ------------------------------------------------------------------- build -----
$variant = if ($Release) { "assembleRelease" } else { "assembleDebug" }
$apkRel  = if ($Release) { "app\build\outputs\apk\release\app-release.apk" } else { "app\build\outputs\apk\debug\app-debug.apk" }
$apk     = Join-Path $root "android\$apkRel"

Push-Location (Join-Path $root "android")
try {
    if (-not $SkipBuild) {
        Step "Building APK ($variant) — first build cross-compiles 3 ABIs, can take ~10 min"
        & .\gradlew.bat --no-daemon ":app:$variant"
        if ($LASTEXITCODE -ne 0) { Fail "Gradle build failed. (UniFFI 'conflicting declarations'? run: .\gradlew.bat clean)" }
    }
    if (-not (Test-Path $apk)) { Fail "APK not found at $apkRel — build it (omit -SkipBuild)." }

    # --------------------------------------------------------- target device ---
    if (-not $Device) {
        $avds = & $emu -list-avds
        if ($avds -notcontains $Avd) { Fail "AVD '$Avd' not found. Create it (see docs) or pass -Device for a USB device." }
        & $emu -accel-check *> $null
        if ($LASTEXITCODE -ne 0) { Warn "Emulator HW acceleration (WHPX) unavailable — boot will be slow. Enable 'Windows Hypervisor Platform'." }
        $running = (& $adb devices) | Select-String -Pattern "^emulator-\d+\s+device"
        if (-not $running) {
            Step "Booting emulator '$Avd' (console output -> %TEMP%\nullgate-emulator-*.log)"
            # Redirected: the emulator spews INFO lines forever, and when this script
            # runs inside another one (scripts\soak\setup-soak.ps1) that spam buried
            # the parent's interactive prompts.
            Start-Process $emu -ArgumentList "-avd",$Avd,"-no-snapshot-load","-no-boot-anim" `
                -RedirectStandardOutput (Join-Path $env:TEMP "nullgate-emulator-out.log") `
                -RedirectStandardError (Join-Path $env:TEMP "nullgate-emulator-err.log")
        } else { Step "Emulator already running" }
    } else {
        $devs = (& $adb devices) | Select-String -Pattern "\sdevice$"
        if (-not $devs) { Fail "No USB device detected. Enable Developer Options + USB debugging and accept the RSA prompt." }
    }

    Step "Waiting for device + boot"
    & $adb wait-for-device
    for ($i = 0; $i -lt 120; $i++) {
        $b = (& $adb shell getprop sys.boot_completed 2>$null | Out-String).Trim()
        if ($b -eq "1") { break }
        Start-Sleep -Seconds 2
    }

    Step "Installing $apkRel"
    & $adb install -r $apk
    if ($LASTEXITCODE -ne 0) { Fail "adb install failed (signature clash? try: adb uninstall $Pkg)." }

    Step "Launching $Act"
    & $adb shell am start -n $Act | Out-Null
    Write-Host "Done. On first connect, grant the VPN permission when the system prompt appears." -ForegroundColor Green
}
finally { Pop-Location }
