# Static sanity check on a bundled Windows distribution: is every binary in it the
# architecture we think it is, and is every DLL it imports actually going to be there
# at runtime?
#
# WHY: the arm64 bundle is cross-built on an x86_64 host, so it cannot simply be
# launched to find out. Both failure modes we care about are visible in the PE headers
# without executing anything:
#   * a stray x64 binary in an arm64 bundle (a target dir mixed up, a helper tool
#     copied from the host tree) — the machine type says so;
#   * a missing runtime DLL — the import table says so, and "app doesn't start, no
#     error" is exactly how that presents to a user.
# It runs for x86_64 too, where it is cheap insurance for the same mistakes.
#
# Usage:  pwsh -File scripts\verify-bundle.ps1 -Dist dist\nullgate-windows-arm64 -Arch arm64

param(
    [Parameter(Mandatory = $true)][string]$Dist,
    [ValidateSet("x86_64", "arm64")][string]$Arch = "x86_64"
)

$ErrorActionPreference = "Stop"

$expected = if ($Arch -eq "arm64") { "ARM64" } else { "x64" }

$objdump = "C:\Program Files\LLVM\bin\llvm-objdump.exe",
           "C:\llvm-mingw-20260616-ucrt-x86_64\bin\llvm-objdump.exe" |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $objdump) { throw "llvm-objdump not found; needed to read PE import tables" }

function Get-PeArch {
    param([string]$Path)
    $fs = [IO.File]::OpenRead($Path)
    try {
        $br = New-Object IO.BinaryReader($fs)
        $fs.Position = 0x3C
        $peOff = $br.ReadInt32()
        $fs.Position = $peOff + 4
        switch ($br.ReadUInt16()) {
            0xAA64  { "ARM64" }
            0x8664  { "x64" }
            0x014C  { "x86" }
            0xA641  { "ARM64EC" }
            default { "unknown" }
        }
    } finally { $fs.Dispose() }
}

Write-Host "Verifying $Dist ($expected)" -ForegroundColor Cyan

$bins = @(Get-ChildItem $Dist -Recurse -Include *.exe, *.dll -File)
if (-not $bins) { throw "no binaries found under $Dist" }

# Everything the bundle carries, by file name — an import satisfied from here travels
# with the app.
$shipped = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($b in $bins) { [void]$shipped.Add($b.Name) }

$problems = @()

# -- 1. Architecture ---------------------------------------------------------
foreach ($b in $bins) {
    $a = Get-PeArch $b.FullName
    if ($a -ne $expected) {
        $problems += "wrong architecture: $($b.FullName.Substring($Dist.Length + 1)) is $a, expected $expected"
    }
}

# -- 2. Imports --------------------------------------------------------------
# A DLL is satisfied if we ship it, or if Windows does. System32 is the right oracle
# for the latter even on this x86_64 host: the ARM64 system carries the same DLL names
# (the arch differs, but presence is what we are asserting).
$sys32 = Join-Path $env:SystemRoot "System32"
$unresolved = @{}

foreach ($b in $bins) {
    $imports = & $objdump -p $b.FullName 2>$null |
        Select-String -Pattern '^\s*DLL Name:\s*(.+)$' |
        ForEach-Object { $_.Matches[0].Groups[1].Value.Trim() }
    foreach ($imp in $imports) {
        if ($shipped.Contains($imp)) { continue }
        # API sets (api-ms-win-*, ext-ms-*) are virtual: the loader maps them to the
        # real host DLL via the apiset schema, and there is no file on disk to find.
        # Looking for them in System32 would report every CRT import as missing.
        if ($imp -like 'api-ms-win-*' -or $imp -like 'ext-ms-*') { continue }
        if (Test-Path (Join-Path $sys32 $imp)) { continue }
        if (-not $unresolved.ContainsKey($imp)) { $unresolved[$imp] = @() }
        $unresolved[$imp] += $b.Name
    }
}

foreach ($imp in $unresolved.Keys | Sort-Object) {
    $by = ($unresolved[$imp] | Select-Object -Unique -First 4) -join ', '
    $problems += "missing DLL: $imp (imported by $by) — not in the bundle and not in System32"
}

# -- Report ------------------------------------------------------------------
if ($problems) {
    Write-Host "FAILED" -ForegroundColor Red
    foreach ($p in $problems) { Write-Host "  $p" -ForegroundColor Red }
    exit 1
}

Write-Host "  $($bins.Count) binaries, all $expected; every imported DLL is bundled or a system DLL" -ForegroundColor Green
exit 0
