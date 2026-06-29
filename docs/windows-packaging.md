# Windows packaging (signed MSI + auto-updater)

How the Windows installer is built, signed, and how it auto-updates. All builds are
**local** (no CI). Run from an elevated-capable shell on Windows.

## What ships
A single **code-signed MSI**, `nullgate-<version>-windows-x86_64.msi`, that:
- installs the self-contained app (the three exes + the full GTK runtime + `wintun.dll`)
  into `C:\Program Files\Nullgate`,
- registers and starts the **LocalSystem `NullgateDaemon`** service (the daemon owns the TUN),
- registers the **`NullgateUpdate`** daily scheduled task (auto-update, runs as SYSTEM),
- adds Start-menu + Desktop shortcuts, and offers "Launch" on finish.

There is no portable zip in a release — the MSI is the Windows artifact. (The bundle step
still produces `dist\nullgate-windows-x86_64.zip` as a byproduct, handy for local testing.)

## Prerequisites (one-time)
- **Rust** (MSVC): `rustup default stable-msvc`.
- **Visual Studio Build Tools** (MSVC C++).
- **GTK4 + libadwaita** via gvsbuild at `C:\gtk` (see `docs/building.md`). `pkg-config` must
  resolve `gtk4` and `libadwaita-1`.
- **WiX 5** dotnet tool: `dotnet tool install --global wix --version "5.*"`. The build script
  adds the UI + Util extensions automatically at the matching engine version.
- **Windows SDK** (provides `signtool.exe`).
- **Azure Trusted Signing client tools** (provide `Azure.CodeSigning.Dlib.dll`) and the
  **Azure CLI** (`az`). See *Signing* below.
- **`gh`** CLI, authenticated, for publishing.

## Build
```powershell
# Stop the service first if it's installed (it locks target\release\nullgate-daemon.exe):
sc.exe stop NullgateDaemon

az login                                  # authenticate the signing session
pwsh -File scripts\build-msi.ps1          # -> target\wix\nullgate-<ver>-windows-x86_64.msi
signtool verify /pa target\wix\nullgate-<ver>-windows-x86_64.msi   # optional check
```
`build-msi.ps1` runs: release build → **sign the exes** → GTK bundle
(`bundle-gtk-windows.ps1`, which also copies the updater into `bin\`) → `wix build`
(`wix\ipn.wxs`) → **sign the MSI**. Version is read from `Cargo.toml`. `-SkipBuild` packages
the existing `target\release` bins; `-Version <x>` overrides the version.

## Signing (Azure Trusted Signing)
Signing is **on by default** and driven by a git-ignored metadata file at the repo root,
`artifact-signing-metadata.json`, plus an interactive `az login` session (no keys on disk).
If the file is absent, the build still succeeds but the artifacts are **unsigned** — never ship
that as a release (SmartScreen will warn).

Create `artifact-signing-metadata.json` (already in `.gitignore`):
```json
{
  "Endpoint": "https://<region>.codesigning.azure.net/",
  "CodeSigningAccountName": "<your-trusted-signing-account>",
  "CertificateProfileName": "<your-cert-profile>",
  "ExcludeCredentials": [
    "EnvironmentCredential", "WorkloadIdentityCredential", "ManagedIdentityCredential",
    "SharedTokenCacheCredential", "VisualStudioCredential", "VisualStudioCodeCredential",
    "AzurePowerShellCredential", "AzureDeveloperCliCredential", "InteractiveBrowserCredential"
  ]
}
```
You need an Azure account with the **"Trusted Signing Certificate Profile Signer"** role on that
account/profile, and `az login` before building. `scripts\sign-artifacts.ps1` finds
`signtool.exe` (latest Windows Kit; override `SIGNTOOL_PATH`) and the dlib (standard Trusted
Signing tool locations; override `ARTIFACT_SIGNING_DLIB`), and timestamps via
`http://timestamp.acs.microsoft.com`. The `ExcludeCredentials` list pins it to the `az login`
session (`AzureCliCredential`) so it doesn't stall on IMDS.

## Auto-update
`packaging\windows\ipn-update.ps1` is installed to `C:\Program Files\Nullgate\bin` and registered as
the SYSTEM scheduled task **`NullgateUpdate`** (daily ~3am ±2h, plus 5 min after boot). It compares
`nullgate-daemon.exe --version` to the latest release tag of the public
`steeb-k/iroh-private-network` repo and, if newer, downloads the MSI and applies it silently
(`msiexec /i … /qn`). The MSI's `MajorUpgrade` stops the service, swaps files, and restarts it.
Logs: `%ProgramData%\ipn\update.log`.

## Gotchas
- A **running `NullgateDaemon` service locks** `nullgate-daemon.exe` — stop it (`sc.exe stop NullgateDaemon`)
  before a release build.
- The MSI is **x64** (`-arch x64`) so it installs under `Program Files`, not `Program Files (x86)`.
- The `UpgradeCode` in `wix\ipn.wxs` is fixed; never change it or upgrades break.
