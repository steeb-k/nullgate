# Fetch a GTK4 + libadwaita stack from the MSYS2 binary repo into a plain
# directory, ready to cross-compile against.
#
# WHY THIS EXISTS (see docs/windows-packaging.md): gvsbuild — where our x86_64
# C:\gtk comes from — is x64-only, and vcpkg's gtk port explicitly excludes
# arm64-windows ("supports": "... & !(arm64 & windows)"). MSYS2's CLANGARM64
# repo is the only source of prebuilt GTK4 + libadwaita for Windows on ARM. Its
# packages are plain .pkg.tar.zst archives served over HTTPS, so we can resolve
# and unpack them on an x86_64 host without MSYS2 or pacman installed — which is
# what lets the whole ARM64 build cross-compile here.
#
# The MSYS2 stack is mingw/LLVM-ABI, not MSVC, so the GUI that links against it
# builds for the aarch64-pc-windows-gnullvm Rust target. That's fine: the GUI and
# the daemon are separate processes that only ever meet over a named pipe, so the
# daemon stays MSVC (see scripts/build-msi.ps1).
#
# Usage:
#   pwsh -File scripts\fetch-gtk-msys2.ps1                     # -> C:\gtk-arm64
#   pwsh -File scripts\fetch-gtk-msys2.ps1 -Root D:\gtk-arm64 -Force

param(
    # MSYS2 environment to pull. clangarm64 = ARM64; clang64/ucrt64 = x86_64.
    [string]$Env = "clangarm64",
    [string]$Root = "C:\gtk-arm64",
    # Roots of the dependency closure. Everything else is pulled in transitively.
    [string[]]$Packages = @("gtk4", "libadwaita", "librsvg", "adwaita-icon-theme"),
    # Re-download packages even if they're already in the cache.
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# clangarm64 packages are named mingw-w64-clang-aarch64-<pkg>.
$prefixes = @{
    "clangarm64" = "mingw-w64-clang-aarch64-"
    "clang64"    = "mingw-w64-clang-x86_64-"
    "ucrt64"     = "mingw-w64-ucrt-x86_64-"
    "mingw64"    = "mingw-w64-x86_64-"
}
if (-not $prefixes.ContainsKey($Env)) { throw "unknown MSYS2 environment '$Env'" }
$pkgPrefix = $prefixes[$Env]
$base      = "https://repo.msys2.org/mingw/$Env"

$cache = Join-Path $env:LOCALAPPDATA "msys2-pkg-cache\$Env"
$tmp   = Join-Path ([IO.Path]::GetTempPath()) "msys2-fetch-$Env"
New-Item -ItemType Directory -Force -Path $cache | Out-Null
if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

Write-Host "MSYS2 $Env -> $Root" -ForegroundColor Cyan

# -- 1. Repo database: the package index, with each package's deps ------------
# <env>.db is a gzipped tar of <name>-<ver>/desc records.
Write-Host "[1/4] fetching the $Env package database" -ForegroundColor Cyan
$dbFile = Join-Path $tmp "db.tar.gz"
Invoke-WebRequest -Uri "$base/$Env.db" -OutFile $dbFile -UseBasicParsing
$dbDir = Join-Path $tmp "db"
New-Item -ItemType Directory -Force -Path $dbDir | Out-Null
& tar -xzf $dbFile -C $dbDir
if ($LASTEXITCODE -ne 0) { throw "failed to extract $Env.db" }

# A desc record is %FIELD% lines followed by values until a blank line.
function Read-Desc {
    param([string]$Path)
    $fields = @{}
    $current = $null
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        if ($line -match '^%(.+)%$') { $current = $matches[1]; $fields[$current] = @(); continue }
        if ([string]::IsNullOrWhiteSpace($line)) { $current = $null; continue }
        if ($current) { $fields[$current] += $line }
    }
    return $fields
}

$pkgs     = @{}   # name -> @{ FileName; Depends }
$provides = @{}   # virtual/provided name -> real package name
foreach ($dir in Get-ChildItem $dbDir -Directory) {
    $desc = Join-Path $dir.FullName "desc"
    if (-not (Test-Path $desc)) { continue }
    $f = Read-Desc $desc
    $name = $f["NAME"] | Select-Object -First 1
    if (-not $name) { continue }
    # Dependency entries can carry version constraints (foo>=1.2) — strip them.
    $deps = @($f["DEPENDS"]) | Where-Object { $_ } | ForEach-Object { ($_ -split '[<>=]')[0].Trim() }
    $pkgs[$name] = @{ FileName = ($f["FILENAME"] | Select-Object -First 1); Depends = $deps }
    foreach ($p in @($f["PROVIDES"]) | Where-Object { $_ }) {
        $provides[($p -split '[<>=]')[0].Trim()] = $name
    }
}
Write-Host "  $($pkgs.Count) packages indexed" -ForegroundColor DarkGray

# -- 2. Transitive closure over the roots ------------------------------------
Write-Host "[2/4] resolving the dependency closure" -ForegroundColor Cyan
$want    = [System.Collections.Generic.HashSet[string]]::new()
$queue   = [System.Collections.Generic.Queue[string]]::new()
$missing = @()
foreach ($p in $Packages) { $queue.Enqueue("$pkgPrefix$p") }

while ($queue.Count -gt 0) {
    $name = $queue.Dequeue()
    if (-not $pkgs.ContainsKey($name)) {
        # Might be a virtual provide (e.g. a -cc-libs alias) rather than a real package.
        if ($provides.ContainsKey($name)) { $name = $provides[$name] } else { $missing += $name; continue }
    }
    if (-not $want.Add($name)) { continue }
    foreach ($d in $pkgs[$name].Depends) {
        # MSYS2 packages depend only within their own environment; anything else
        # (e.g. a msys/ runtime dep) is not something we link or ship.
        if ($d.StartsWith($pkgPrefix)) { $queue.Enqueue($d) }
    }
}
if ($missing) { Write-Host "  note: not in this repo, skipped: $($missing -join ', ')" -ForegroundColor DarkYellow }
Write-Host "  $($want.Count) packages to fetch" -ForegroundColor DarkGray

# -- 3. Download + unpack ----------------------------------------------------
Write-Host "[3/4] downloading + unpacking" -ForegroundColor Cyan
$stage = Join-Path $tmp "stage"
New-Item -ItemType Directory -Force -Path $stage | Out-Null

$unpackWarnings = @()
$i = 0
foreach ($name in ($want | Sort-Object)) {
    $i++
    $file = $pkgs[$name].FileName
    if (-not $file) { continue }
    $local = Join-Path $cache $file
    if ($Force -or -not (Test-Path $local)) {
        Write-Host ("  [{0,3}/{1}] {2}" -f $i, $want.Count, $file) -ForegroundColor DarkGray
        Invoke-WebRequest -Uri "$base/$file" -OutFile $local -UseBasicParsing
    } else {
        Write-Host ("  [{0,3}/{1}] {2} (cached)" -f $i, $want.Count, $file) -ForegroundColor DarkGray
    }
    # bsdtar (shipped with Windows) reads .zst natively. Every payload path is
    # prefixed with the environment name; the metadata files (.PKGINFO etc.) are
    # not, so unpacking the whole archive and taking only <env>/ drops them.
    #
    # The excluded trees are documentation and translation data we never ship —
    # and, not incidentally, the only places these packages use symlinks (e.g.
    # iso-codes' iso_3166.mo -> iso_3166-1.mo). bsdtar can't create a symlink
    # without Developer Mode, so leaving them in makes tar exit nonzero on a
    # machine that is otherwise perfectly able to build. Dropping them also takes
    # a few hundred MB off the tree.
    & tar -xf $local -C $stage `
        --exclude "$Env/share/locale/*" --exclude "$Env/share/doc/*" `
        --exclude "$Env/share/man/*"    --exclude "$Env/share/gtk-doc/*" `
        --exclude "$Env/share/info/*"   --exclude "$Env/share/xml/*"
    if ($LASTEXITCODE -ne 0) { $unpackWarnings += $file }
}
if ($unpackWarnings) {
    Write-Host "  note: tar reported errors unpacking: $($unpackWarnings -join ', ')" -ForegroundColor DarkYellow
    Write-Host "        (harmless if the verification below passes)" -ForegroundColor DarkYellow
}

$payload = Join-Path $stage $Env
if (-not (Test-Path $payload)) { throw "unpacked archives contained no $Env/ payload" }

if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
New-Item -ItemType Directory -Force -Path $Root | Out-Null
Copy-Item -Path (Join-Path $payload "*") -Destination $Root -Recurse -Force

# -- 4. Make the .pc files usable off-MSYS2 ----------------------------------
# They hardcode `prefix=/clangarm64`, an MSYS2-internal path that means nothing
# to a Windows pkgconf. Point it at the real root so the -I/-L flags resolve.
Write-Host "[4/4] rewriting .pc prefixes -> $Root" -ForegroundColor Cyan
$pcRoot  = $Root.Replace('\', '/')
$pcDir   = Join-Path $Root "lib\pkgconfig"
$pattern = '(?m)^prefix\s*=\s*/' + [regex]::Escape($Env) + '\s*$'
$n = 0
foreach ($pc in Get-ChildItem $pcDir -Filter *.pc -ErrorAction SilentlyContinue) {
    $text  = Get-Content -Raw -LiteralPath $pc.FullName
    $fixed = [regex]::Replace($text, $pattern, "prefix=$pcRoot")
    if ($fixed -ne $text) { Set-Content -LiteralPath $pc.FullName -Value $fixed -NoNewline; $n++ }
}
if ($n -eq 0) { throw "no .pc prefixes were rewritten — pkgconf would resolve /$Env, which does not exist on Windows" }
Write-Host "  rewrote $n .pc files" -ForegroundColor DarkGray

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue

# Verify what we actually need is present, rather than trusting tar's exit code —
# some unpack errors above are expected (symlinks) and some would not be.
$required = @(
    "lib\pkgconfig\gtk4.pc",
    "lib\pkgconfig\libadwaita-1.pc",
    "lib\gdk-pixbuf-2.0\2.10.0\loaders",
    "share\glib-2.0\schemas"
)
$absent = $required | Where-Object { -not (Test-Path (Join-Path $Root $_)) }
if ($absent) { throw "the fetched stack is incomplete — missing: $($absent -join ', ')" }

$dlls = (Get-ChildItem (Join-Path $Root "bin") -Filter *.dll -ErrorAction SilentlyContinue).Count
Write-Host "Done -> $Root  ($dlls DLLs, $($want.Count) packages)" -ForegroundColor Green
Write-Host "  PKG_CONFIG_PATH=$Root\lib\pkgconfig" -ForegroundColor Green
