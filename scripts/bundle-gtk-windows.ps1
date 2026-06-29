# Assemble a self-contained Windows distribution of iroh-private-network.
#
# Mirrors seed-sync-gtk's bundling: copies the GTK4/libadwaita runtime (DLLs,
# compiled GSettings schemas, gdk-pixbuf loaders, icon theme) next to ipn.exe so
# the app runs on a machine with no GTK install. Also fetches wintun.dll, which
# tun-rs loads at runtime to bring up the virtual interface (routing).
#
# Prereqs (see docs/windows-packaging.md):
#   * MSVC toolchain, GTK4 + libadwaita via gvsbuild (default C:\gtk)
#
# Usage:  pwsh -File scripts\bundle-gtk-windows.ps1 [-GtkRoot C:\gtk] [-SkipBuild]

param(
    [string]$GtkRoot = "C:\gtk",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$name = "ipn-windows-x86_64"
$dist = Join-Path $root "dist\$name"
$gbin = Join-Path $GtkRoot "bin"
$wintunVer = "0.14.1"

if (-not $SkipBuild) {
    Write-Host "Building release..."
    & cargo build --release -p ipn-gui -p ipn-daemon -p ipn-cli
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

Write-Host "Bundling -> $dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path "$dist\bin" | Out-Null

# 1. Our binaries: GUI (unprivileged), daemon (the service), CLI.
Copy-Item "$root\target\release\ipn.exe" "$dist\bin\"
Copy-Item "$root\target\release\ipn-daemon.exe" "$dist\bin\"
Copy-Item "$root\target\release\ipn-cli.exe" "$dist\bin\"

# Auto-update engine (the MSI installs it next to the exes and registers a daily
# SYSTEM scheduled task that runs it; harmless in the portable zip too).
Copy-Item "$root\packaging\windows\ipn-update.ps1" "$dist\bin\"

# 2. wintun.dll (cached in vendor\wintun). tun-rs loads it at runtime; without it
#    the app still runs but routing stays off. Wintun is proprietary (WireGuard
#    LLC) — we ship its LICENSE.txt alongside the DLL, as its license requires and
#    as our GPL "Wintun exception" assumes (see LICENSE).
$wintunDir = Join-Path $root "vendor\wintun"
$wintunDll = Join-Path $wintunDir "wintun.dll"
$wintunLic = Join-Path $wintunDir "LICENSE.txt"
if (-not (Test-Path $wintunDll) -or -not (Test-Path $wintunLic)) {
    New-Item -ItemType Directory -Force -Path $wintunDir | Out-Null
    $zip = Join-Path $env:TEMP "wintun-$wintunVer.zip"
    Write-Host "Fetching wintun $wintunVer..."
    Invoke-WebRequest -Uri "https://www.wintun.net/builds/wintun-$wintunVer.zip" -OutFile $zip
    $extract = Join-Path $env:TEMP "wintun-$wintunVer"
    if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    Expand-Archive -Path $zip -DestinationPath $extract
    Copy-Item "$extract\wintun\bin\amd64\wintun.dll" $wintunDll
    Copy-Item "$extract\wintun\LICENSE.txt" $wintunLic
}
Copy-Item $wintunDll "$dist\bin\"

# Licenses in the bundle: this project (GPLv3 + Wintun exception) and Wintun's own.
Copy-Item "$root\LICENSE" "$dist\LICENSE.txt"
Copy-Item $wintunLic "$dist\wintun-LICENSE.txt"

# 3. GTK runtime DLLs.
Copy-Item "$gbin\*.dll" "$dist\bin\"
if (Test-Path "$gbin\gdbus.exe") { Copy-Item "$gbin\gdbus.exe" "$dist\bin\" }

# 4. Compiled GSettings schemas (libadwaita aborts without these).
$schemas = "$dist\share\glib-2.0\schemas"
New-Item -ItemType Directory -Force -Path $schemas | Out-Null
Copy-Item "$GtkRoot\share\glib-2.0\schemas\*.xml" $schemas -ErrorAction SilentlyContinue
& "$gbin\glib-compile-schemas.exe" $schemas

# 5. gdk-pixbuf loaders (+ relocatable cache) for PNG/SVG icons.
$loaders = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders"
New-Item -ItemType Directory -Force -Path $loaders | Out-Null
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" $loaders
$cache = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"
$cacheDir = (Split-Path $cache).Replace('\', '/')
[string[]]$loaderNames = Get-ChildItem "$loaders\*.dll" | Select-Object -ExpandProperty Name
Push-Location $loaders
$cacheText = & "$gbin\gdk-pixbuf-query-loaders.exe" $loaderNames
Pop-Location
($cacheText | ForEach-Object { $_.Replace("$cacheDir/", '') }) | Set-Content -Encoding ASCII $cache

# 6. Icon themes (+ cache).
New-Item -ItemType Directory -Force -Path "$dist\share\icons" | Out-Null
Copy-Item -Recurse "$GtkRoot\share\icons\Adwaita" "$dist\share\icons\" -ErrorAction SilentlyContinue
Copy-Item -Recurse "$GtkRoot\share\icons\hicolor" "$dist\share\icons\" -ErrorAction SilentlyContinue
if (Test-Path "$gbin\gtk-update-icon-cache.exe") {
    & "$gbin\gtk-update-icon-cache.exe" "$dist\share\icons\Adwaita"
}

# 7. Setup scripts. The DAEMON runs as a LocalSystem service (owns the TUN); the
#    GUI runs unprivileged. Install the service once (elevated), then just run the
#    GUI normally — no per-launch elevation.
$install = @'
@echo off
REM Install + start the IPN daemon as a Windows service (one-time, elevated).
set HERE=%~dp0
powershell -Command "Start-Process -FilePath '%HERE%bin\ipn-daemon.exe' -ArgumentList 'install' -Verb RunAs"
echo If you approved the UAC prompt, the IPN service is now running.
echo Now launch IPN.bat (no elevation needed).
pause
'@
Set-Content -Path "$dist\1. Install service (admin).bat" -Value $install -Encoding ASCII

$uninstall = @'
@echo off
set HERE=%~dp0
powershell -Command "Start-Process -FilePath '%HERE%bin\ipn-daemon.exe' -ArgumentList 'uninstall' -Verb RunAs"
pause
'@
Set-Content -Path "$dist\Uninstall service (admin).bat" -Value $uninstall -Encoding ASCII

$gui = @'
@echo off
set HERE=%~dp0
start "" "%HERE%bin\ipn.exe"
'@
Set-Content -Path "$dist\2. IPN.bat" -Value $gui -Encoding ASCII

# 8. Zip it.
$zipOut = Join-Path $root "dist\$name.zip"
if (Test-Path $zipOut) { Remove-Item -Force $zipOut }
Compress-Archive -Path "$dist\*" -DestinationPath $zipOut
Write-Host "Done: $zipOut"
Write-Host "Run bin\ipn.exe (or the .bat to elevate for routing)."
