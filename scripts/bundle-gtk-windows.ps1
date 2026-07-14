# Assemble a self-contained Windows distribution of Nullgate, for x86_64 or arm64.
#
# Mirrors seed-sync-gtk's bundling: copies the GTK4/libadwaita runtime (DLLs,
# compiled GSettings schemas, gdk-pixbuf loaders, icon theme) next to nullgate.exe so
# the app runs on a machine with no GTK install. Also fetches wintun.dll, which
# tun-rs loads at runtime to bring up the virtual interface (routing).
#
# The two architectures get their GTK from different places, because they have to
# (see docs/windows-packaging.md):
#   x86_64 : gvsbuild        (C:\gtk)        — MSVC ABI, all three exes are MSVC.
#   arm64  : MSYS2 CLANGARM64 (C:\gtk-arm64) — gvsbuild is x64-only and vcpkg's gtk
#            port excludes arm64-windows, so MSYS2 is the only prebuilt GTK4 +
#            libadwaita for Windows on ARM. It is mingw-ABI, so the GUI is built for
#            aarch64-pc-windows-gnullvm while the daemon/CLI stay MSVC. That mix is
#            harmless: they are separate processes that only meet over a named pipe.
#
# Cross-building arm64 on an x86_64 host means the GTK helper *tools* in the arm64
# tree (glib-compile-schemas, gdk-pixbuf-query-loaders, ...) cannot run here. They
# produce arch-independent output, so we run the x86_64 build of the very same MSYS2
# packages instead (-HostToolsRoot, fetched with `fetch-gtk-msys2.ps1 -Env ucrt64`).
# Same upstream version, same loader set => same generated caches.
#
# Prereqs (see docs/windows-packaging.md):
#   x86_64: MSVC toolchain, GTK4 + libadwaita via gvsbuild (default C:\gtk)
#   arm64 : pwsh -File scripts\fetch-gtk-msys2.ps1                      # -> C:\gtk-arm64
#           pwsh -File scripts\fetch-gtk-msys2.ps1 -Env ucrt64 -Root C:\gtk-msys2-x64
#           plus llvm-mingw on PATH (aarch64-w64-mingw32-clang) for the GUI link.
#
# Usage:  pwsh -File scripts\bundle-gtk-windows.ps1 [-Arch x86_64|arm64] [-SkipBuild]

param(
    [ValidateSet("x86_64", "arm64")]
    [string]$Arch = "x86_64",
    [string]$GtkRoot,
    [string]$HostToolsRoot,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$name = "nullgate-windows-$Arch"
$dist = Join-Path $root "dist\$name"
$wintunVer = "0.14.1"

# Per-arch layout. The arm64 GUI and daemon come out of *different* target dirs
# because they are built for different ABIs (gnullvm vs msvc) — see the header.
if ($Arch -eq "arm64") {
    if (-not $GtkRoot)       { $GtkRoot = "C:\gtk-arm64" }
    if (-not $HostToolsRoot) { $HostToolsRoot = "C:\gtk-msys2-x64" }
    $guiDir    = Join-Path $root "target\aarch64-pc-windows-gnullvm\release"
    $svcDir    = Join-Path $root "target\aarch64-pc-windows-msvc\release"
    $wintunArc = "arm64"
} else {
    if (-not $GtkRoot)       { $GtkRoot = "C:\gtk" }
    if (-not $HostToolsRoot) { $HostToolsRoot = $GtkRoot }
    $guiDir    = Join-Path $root "target\release"
    $svcDir    = $guiDir
    $wintunArc = "amd64"
}
$gbin  = Join-Path $GtkRoot "bin"
$hbin  = Join-Path $HostToolsRoot "bin"

if (-not (Test-Path $gbin)) { throw "no GTK runtime at $GtkRoot (see the prereqs in this script's header)" }
if (-not (Test-Path $hbin)) { throw "no host GTK tools at $HostToolsRoot (see the prereqs in this script's header)" }

# The host tools are dynamically linked against their own tree.
$env:PATH = "$hbin;$env:PATH"

if (-not $SkipBuild) {
    Write-Host "Building release ($Arch)..."
    if ($Arch -eq "arm64") {
        & "$root\scripts\build-arm64.ps1"
        if ($LASTEXITCODE -ne 0) { throw "arm64 build failed" }
    } else {
        & cargo build --release -p ipn-gui -p ipn-daemon -p ipn-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }
}

Write-Host "Bundling -> $dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path "$dist\bin" | Out-Null

# 1. Our binaries: GUI (unprivileged), daemon (the service), CLI.
Copy-Item "$guiDir\nullgate.exe" "$dist\bin\"
Copy-Item "$svcDir\nullgate-daemon.exe" "$dist\bin\"
Copy-Item "$svcDir\nullgate-cli.exe" "$dist\bin\"

# Auto-update engine (the MSI installs it next to the exes and registers a daily
# SYSTEM scheduled task that runs it; harmless in the portable zip too).
Copy-Item "$root\packaging\windows\nullgate-update.ps1" "$dist\bin\"

# 2. wintun.dll (cached in vendor\wintun\<arch>). tun-rs loads it at runtime; without
#    it the app still runs but routing stays off. The DLL is arch-specific and this is
#    the one piece that genuinely cannot be emulated — an x64 wintun.dll on an ARM64
#    machine would try to install an x64 kernel driver, which an ARM64 kernel will not
#    load. That is the whole reason a native ARM64 build exists. Wintun is proprietary
#    (WireGuard LLC) — we ship its LICENSE.txt alongside the DLL, as its license
#    requires and as our GPL "Wintun exception" assumes (see LICENSE).
$wintunDir = Join-Path $root "vendor\wintun\$wintunArc"
$wintunDll = Join-Path $wintunDir "wintun.dll"
$wintunLic = Join-Path $wintunDir "LICENSE.txt"
if (-not (Test-Path $wintunDll) -or -not (Test-Path $wintunLic)) {
    New-Item -ItemType Directory -Force -Path $wintunDir | Out-Null
    $zip = Join-Path $env:TEMP "wintun-$wintunVer.zip"
    if (-not (Test-Path $zip)) {
        Write-Host "Fetching wintun $wintunVer..."
        Invoke-WebRequest -Uri "https://www.wintun.net/builds/wintun-$wintunVer.zip" -OutFile $zip
    }
    $extract = Join-Path $env:TEMP "wintun-$wintunVer"
    if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    Expand-Archive -Path $zip -DestinationPath $extract
    Copy-Item "$extract\wintun\bin\$wintunArc\wintun.dll" $wintunDll
    Copy-Item "$extract\wintun\LICENSE.txt" $wintunLic
}
Copy-Item $wintunDll "$dist\bin\"

# Licenses in the bundle: this project (GPLv3 + Wintun exception) and Wintun's own.
Copy-Item "$root\LICENSE" "$dist\LICENSE.txt"
Copy-Item $wintunLic "$dist\wintun-LICENSE.txt"

# 3. GTK runtime DLLs.
if (Test-Path "$gbin\gdbus.exe") { Copy-Item "$gbin\gdbus.exe" "$dist\bin\" }

if ($Arch -eq "arm64") {
    # MSYS2's bin\ is a shared prefix for all ~90 packages in the dependency closure,
    # not a purpose-built GTK tree like gvsbuild's — copying *.dll from it drags in
    # things like libpython3.14.dll that nothing in the app ever loads. So take only
    # what is actually reachable: walk the import tables from our own binaries (plus
    # the pixbuf loaders, which GTK dlopen()s rather than imports) and copy the
    # transitive closure. Anything not in the GTK tree is a system DLL and is skipped
    # by construction, because we only ever copy names we find in $gbin.
    $mingwDump = (Get-ChildItem "C:\" -Directory -Filter "llvm-mingw-*" -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1)
    $objdump = @(
        "C:\Program Files\LLVM\bin\llvm-objdump.exe"
        if ($mingwDump) { Join-Path $mingwDump.FullName "bin\llvm-objdump.exe" }
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $objdump) { throw "llvm-objdump not found; needed to resolve the GTK import closure" }

    $seeds = @(Get-ChildItem "$dist\bin\*.exe" -File) +
             @(Get-ChildItem "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" -File)

    $copied = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $queue  = [System.Collections.Generic.Queue[string]]::new()
    foreach ($s in $seeds) { $queue.Enqueue($s.FullName) }

    while ($queue.Count -gt 0) {
        $pe = $queue.Dequeue()
        $imports = & $objdump -p $pe 2>$null |
            Select-String -Pattern '^\s*DLL Name:\s*(.+)$' |
            ForEach-Object { $_.Matches[0].Groups[1].Value.Trim() }
        foreach ($imp in $imports) {
            $src = Join-Path $gbin $imp
            if (-not (Test-Path $src)) { continue }        # a system DLL, or not ours to ship
            if (-not $copied.Add($imp)) { continue }
            Copy-Item $src "$dist\bin\"
            $queue.Enqueue($src)
        }
    }
    Write-Host "  GTK import closure: $($copied.Count) DLLs" -ForegroundColor DarkGray
} else {
    # gvsbuild's tree is already just the GTK stack, so take it wholesale — this is
    # the shipping x86_64 path and is left exactly as it was.
    Copy-Item "$gbin\*.dll" "$dist\bin\"
}

# 4. Compiled GSettings schemas (libadwaita aborts without these). The compiler
#    reads XML and writes a GVariant blob — no target code involved, so the host
#    build of the same glib produces a byte-identical result.
$schemas = "$dist\share\glib-2.0\schemas"
New-Item -ItemType Directory -Force -Path $schemas | Out-Null
Copy-Item "$GtkRoot\share\glib-2.0\schemas\*.xml" $schemas -ErrorAction SilentlyContinue
& "$hbin\glib-compile-schemas.exe" $schemas
if ($LASTEXITCODE -ne 0) { throw "glib-compile-schemas failed" }

# 5. gdk-pixbuf loaders (+ relocatable cache) for PNG/SVG icons.
#    query-loaders g_module_open()s every loader to read its metadata, so it can only
#    ever query loaders of its OWN architecture. We therefore query the host tree's
#    loaders and ship the arm64 ones: same package, same version, same module set (the
#    build asserts that below), so the only difference between the two caches would be
#    the paths — and those we rewrite to be relative anyway.
$loaders = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders"
New-Item -ItemType Directory -Force -Path $loaders | Out-Null
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" $loaders

$hostLoaders = "$HostToolsRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders"
$shipNames = (Get-ChildItem "$loaders\*.dll"     | Select-Object -ExpandProperty Name | Sort-Object) -join ','
$hostNames = (Get-ChildItem "$hostLoaders\*.dll" | Select-Object -ExpandProperty Name | Sort-Object) -join ','
if ($shipNames -ne $hostNames) {
    throw "the host GTK tree's pixbuf loaders differ from the ones being shipped; the generated loaders.cache would be wrong.`n  shipped: $shipNames`n  host   : $hostNames"
}

$cache = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"
$hostCacheDir = (Split-Path $hostLoaders).Replace('\', '/')
[string[]]$loaderNames = Get-ChildItem "$hostLoaders\*.dll" | Select-Object -ExpandProperty Name
Push-Location $hostLoaders
$cacheText = & "$hbin\gdk-pixbuf-query-loaders.exe" $loaderNames
Pop-Location
if (-not $cacheText) { throw "gdk-pixbuf-query-loaders produced nothing" }
($cacheText | ForEach-Object { $_.Replace("$hostCacheDir/", '') }) | Set-Content -Encoding ASCII $cache

# 6. Icon themes (+ cache). gvsbuild ships gtk-update-icon-cache; MSYS2 names it
#    gtk4-update-icon-cache. The cache is arch-independent, so the host tool is fine.
New-Item -ItemType Directory -Force -Path "$dist\share\icons" | Out-Null
Copy-Item -Recurse "$GtkRoot\share\icons\Adwaita" "$dist\share\icons\" -ErrorAction SilentlyContinue
Copy-Item -Recurse "$GtkRoot\share\icons\hicolor" "$dist\share\icons\" -ErrorAction SilentlyContinue
$iconTool = "$hbin\gtk4-update-icon-cache.exe", "$hbin\gtk-update-icon-cache.exe" |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if ($iconTool -and (Test-Path "$dist\share\icons\Adwaita\index.theme")) {
    & $iconTool "$dist\share\icons\Adwaita"
}

# 7. Setup scripts. The DAEMON runs as a LocalSystem service (owns the TUN); the
#    GUI runs unprivileged. Install the service once (elevated), then just run the
#    GUI normally — no per-launch elevation.
$install = @'
@echo off
REM Install + start the Nullgate daemon as a Windows service (one-time, elevated).
set HERE=%~dp0
powershell -Command "Start-Process -FilePath '%HERE%bin\nullgate-daemon.exe' -ArgumentList 'install' -Verb RunAs"
echo If you approved the UAC prompt, the Nullgate service is now running.
echo Now launch Nullgate.bat (no elevation needed).
pause
'@
Set-Content -Path "$dist\1. Install service (admin).bat" -Value $install -Encoding ASCII

$uninstall = @'
@echo off
set HERE=%~dp0
powershell -Command "Start-Process -FilePath '%HERE%bin\nullgate-daemon.exe' -ArgumentList 'uninstall' -Verb RunAs"
pause
'@
Set-Content -Path "$dist\Uninstall service (admin).bat" -Value $uninstall -Encoding ASCII

$gui = @'
@echo off
set HERE=%~dp0
start "" "%HERE%bin\nullgate.exe"
'@
Set-Content -Path "$dist\2. Nullgate.bat" -Value $gui -Encoding ASCII

# 8. Check the bundle is complete and single-architecture.
#    This matters most for arm64, which we cannot launch on an x86_64 build host: a
#    missing DLL or a stray x64 binary would otherwise only surface as "the app won't
#    start" on a user's machine. Reading the PE headers proves both without running
#    anything.
& "$root\scripts\verify-bundle.ps1" -Dist $dist -Arch $Arch
if ($LASTEXITCODE -ne 0) { throw "bundle verification failed" }

# 9. Zip it.
$zipOut = Join-Path $root "dist\$name.zip"
if (Test-Path $zipOut) { Remove-Item -Force $zipOut }
Compress-Archive -Path "$dist\*" -DestinationPath $zipOut
Write-Host "Done: $zipOut"
Write-Host "Run bin\nullgate.exe (or the .bat to elevate for routing)."
