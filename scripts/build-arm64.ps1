# Cross-build the three Nullgate binaries for Windows on ARM, from an x86_64 host.
#
# The ARM64 build is split across two ABIs, on purpose:
#
#   daemon + CLI -> aarch64-pc-windows-msvc      (no GTK dependency at all)
#   GUI          -> aarch64-pc-windows-gnullvm   (mingw ABI, to match MSYS2's GTK)
#
# The split is forced by GTK: gvsbuild (our x86_64 C:\gtk) is x64-only and vcpkg's gtk
# port explicitly excludes arm64-windows, leaving MSYS2's CLANGARM64 packages as the
# only prebuilt GTK4 + libadwaita for Windows on ARM — and those are mingw-ABI. It
# costs us nothing: the GUI and the daemon are separate processes that only ever meet
# over a named pipe, so no ABI boundary is ever crossed inside a process.
#
# Prereqs:
#   * llvm-mingw (https://github.com/mstorsjo/llvm-mingw/releases, ucrt-x86_64) at
#     C:\llvm-mingw-*  — supplies aarch64-w64-mingw32-{clang,windres} for the GUI.
#   * LLVM/clang on PATH or at C:\Program Files\LLVM — `ring` requires clang to build
#     its aarch64 assembly (this is the one dep that does).
#   * The ARM64 GTK stack + the x86_64 host-tools mirror:
#       pwsh -File scripts\fetch-gtk-msys2.ps1
#       pwsh -File scripts\fetch-gtk-msys2.ps1 -Env ucrt64 -Root C:\gtk-msys2-x64
#   * rustup target add aarch64-pc-windows-msvc aarch64-pc-windows-gnullvm
#   * MSVC ARM64 cross tools + the ARM64 Windows SDK (VS installer:
#     "MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools").
#
# Usage:  pwsh -File scripts\build-arm64.ps1 [-GtkRoot C:\gtk-arm64]

param(
    [string]$GtkRoot = "C:\gtk-arm64",
    [string]$PkgConfig
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$llvmMingw = Get-ChildItem "C:\" -Directory -Filter "llvm-mingw-*" -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1
if (-not $llvmMingw) {
    throw "llvm-mingw not found at C:\llvm-mingw-*. Download the ucrt-x86_64 build from https://github.com/mstorsjo/llvm-mingw/releases and unzip it to C:\."
}
if (-not (Test-Path $GtkRoot)) {
    throw "no ARM64 GTK stack at $GtkRoot. Run: pwsh -File scripts\fetch-gtk-msys2.ps1"
}

# pkgconf itself is a host tool — any x86_64 build will do. gvsbuild ships one.
if (-not $PkgConfig) {
    $PkgConfig = "C:\gtk\bin\pkgconf.exe", "C:\gtk\bin\pkg-config.exe" |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $PkgConfig) { throw "no pkgconf/pkg-config found (looked in C:\gtk\bin). Pass -PkgConfig <path>." }

# Point pkg-config at the ARM64 stack ONLY. Setting PKG_CONFIG_LIBDIR (not just _PATH)
# is what stops it falling back to the host's x86_64 .pc files and silently handing the
# linker x64 import libraries.
$env:PKG_CONFIG           = $PkgConfig
$env:PKG_CONFIG_PATH      = "$GtkRoot\lib\pkgconfig"
$env:PKG_CONFIG_LIBDIR    = "$GtkRoot\lib\pkgconfig"
$env:PKG_CONFIG_ALLOW_CROSS = "1"

$env:CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER = "aarch64-w64-mingw32-clang"

# The two passes need DIFFERENT clangs, and the order on PATH is what selects them.
# `ring` compiles C for both targets, so whichever `clang` it finds first has to be the
# one that can serve that pass:
#   * msvc pass    -> LLVM's clang, which discovers the MSVC/Windows SDK headers itself.
#     llvm-mingw's clang cannot: it targets mingw and has no idea where assert.h lives,
#     and the build dies with "'assert.h' file not found".
#   * gnullvm pass -> llvm-mingw's clang, which brings its own mingw sysroot.
# So don't collapse these into one PATH — a single ordering breaks one pass or the other.
$basePath  = $env:PATH
$msvcPath  = "C:\Program Files\LLVM\bin;$basePath"
$mingwPath = "$($llvmMingw.FullName)\bin;C:\Program Files\LLVM\bin;$basePath"

Push-Location $root
try {
    Write-Host "[1/2] daemon + CLI  -> aarch64-pc-windows-msvc" -ForegroundColor Cyan
    $env:PATH = $msvcPath
    & cargo build --release --target aarch64-pc-windows-msvc -p ipn-daemon -p ipn-cli
    if ($LASTEXITCODE -ne 0) { throw "cargo build (msvc) failed" }

    Write-Host "[2/2] GUI           -> aarch64-pc-windows-gnullvm" -ForegroundColor Cyan
    $env:PATH = $mingwPath
    & cargo build --release --target aarch64-pc-windows-gnullvm -p ipn-gui
    if ($LASTEXITCODE -ne 0) { throw "cargo build (gnullvm) failed" }
} finally {
    $env:PATH = $basePath
    Pop-Location
}

Write-Host "Done:" -ForegroundColor Green
Write-Host "  target\aarch64-pc-windows-gnullvm\release\nullgate.exe" -ForegroundColor Green
Write-Host "  target\aarch64-pc-windows-msvc\release\nullgate-daemon.exe" -ForegroundColor Green
Write-Host "  target\aarch64-pc-windows-msvc\release\nullgate-cli.exe" -ForegroundColor Green
