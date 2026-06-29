<#
.SYNOPSIS
  Sign one or more files with Azure Trusted Signing (Azure Code Signing).

  Uses signtool + the Azure.CodeSigning Dlib and a metadata JSON that names the
  signing account + certificate profile. Used by scripts\build-msi.ps1 to sign our
  exes (before wix build) and the MSI (after).

.PARAMETER Files
  One or more paths to sign.

.PARAMETER MetadataPath
  The Azure signing metadata JSON ({ Endpoint, CodeSigningAccountName,
  CertificateProfileName }). Defaults to $env:ARTIFACT_SIGNING_METADATA, else a
  repo-root artifact-signing-metadata.json. If absent, signing is SKIPPED (the
  build still succeeds, producing unsigned artifacts) so other devs / CI can build.

.NOTES
  Auth is via an interactive Azure session (az login) — no keys on disk. Tools
  resolved automatically: signtool.exe from the latest Windows Kit (override with
  $env:SIGNTOOL_PATH), and Azure.CodeSigning.Dlib.dll from the Trusted Signing
  client tools (override with $env:ARTIFACT_SIGNING_DLIB). See
  docs/windows-packaging.md for the full setup.
#>
param(
    [Parameter(Mandatory = $true)] [string[]]$Files,
    [string]$MetadataPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $MetadataPath) {
    $MetadataPath = if ($env:ARTIFACT_SIGNING_METADATA) { $env:ARTIFACT_SIGNING_METADATA }
                    else { Join-Path $root 'artifact-signing-metadata.json' }
}

if (-not (Test-Path $MetadataPath)) {
    Write-Host "sign-artifacts: no signing metadata at '$MetadataPath' — skipping signing (artifacts will be UNSIGNED)." -ForegroundColor Yellow
    return
}

# --- resolve signtool.exe ---------------------------------------------------
$SignTool = $env:SIGNTOOL_PATH
if (-not $SignTool -or -not (Test-Path $SignTool)) {
    $KitsBin = "C:\Program Files (x86)\Windows Kits\10\bin"
    if (Test-Path $KitsBin) {
        $latest = Get-ChildItem $KitsBin -Directory |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+' } |
            Sort-Object { [version]($_.Name -replace '^(\d+\.\d+\.\d+).*', '$1') } -Descending |
            Select-Object -First 1
        if ($latest) {
            $cand = Join-Path $latest.FullName 'x64\signtool.exe'
            if (Test-Path $cand) { $SignTool = $cand }
        }
    }
    if (-not $SignTool) {
        $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
        if ($cmd) { $SignTool = $cmd.Source }
    }
}
if (-not $SignTool -or -not (Test-Path $SignTool)) {
    throw "sign-artifacts: signtool.exe not found. Install the Windows SDK or set SIGNTOOL_PATH."
}

# --- resolve the Azure Code Signing Dlib ------------------------------------
$Dlib = $env:ARTIFACT_SIGNING_DLIB
if (-not $Dlib -or -not (Test-Path $Dlib)) {
    $roots = @(
        "$env:LOCALAPPDATA\Microsoft\MicrosoftArtifactSigningClientTools",
        "$env:LOCALAPPDATA\Microsoft\TrustedSigningClientTools",
        "C:\ProgramData\Microsoft\MicrosoftTrustedSigningClientTools",
        "C:\Program Files\Microsoft\Azure Artifact Signing Client Tools",
        "C:\Program Files (x86)\Microsoft\Azure Artifact Signing Client Tools",
        "C:\Program Files (x86)\Windows Kits\AzureCodeSigning"
    )
    foreach ($r in $roots) {
        if (Test-Path $r) {
            $found = Get-ChildItem -Path $r -Recurse -Filter 'Azure.CodeSigning.Dlib.dll' -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found) { $Dlib = $found.FullName; break }
        }
    }
}
if (-not $Dlib -or -not (Test-Path $Dlib)) {
    throw "sign-artifacts: Azure.CodeSigning.Dlib.dll not found. Install the Trusted Signing client tools or set ARTIFACT_SIGNING_DLIB."
}

Write-Host "sign-artifacts: signtool=$SignTool" -ForegroundColor DarkGray
Write-Host "sign-artifacts: dlib=$Dlib" -ForegroundColor DarkGray

foreach ($f in $Files) {
    if (-not (Test-Path $f)) { throw "sign-artifacts: file not found: $f" }
    Write-Host "Signing $f" -ForegroundColor Cyan
    & $SignTool sign /v /fd SHA256 `
        /tr http://timestamp.acs.microsoft.com /td SHA256 `
        /dlib $Dlib /dmdf $MetadataPath `
        $f
    if ($LASTEXITCODE -ne 0) { throw "sign-artifacts: signtool failed (exit $LASTEXITCODE) for $f" }
}
