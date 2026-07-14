# Windows packaging (signed MSI + auto-updater)

How the Windows installer is built, signed, and how it auto-updates. All builds are
**local** (no CI). Run from an elevated-capable shell on Windows.

## What ships
Two **code-signed MSIs** — `nullgate-<version>-windows-x86_64.msi` and
`nullgate-<version>-windows-arm64.msi` — each of which:
- installs the self-contained app (the three exes + the full GTK runtime + `wintun.dll`)
  into `C:\Program Files\Nullgate`,
- registers and starts the **LocalSystem `NullgateDaemon`** service (the daemon owns the TUN),
- registers the **`NullgateUpdate`** daily scheduled task (auto-update, runs as SYSTEM),
- adds Start-menu + Desktop shortcuts, and offers "Launch" on finish.

Both are built **on an x86_64 host** — the ARM64 one is fully cross-compiled (see below).
They share an `UpgradeCode`, so installing the ARM64 MSI on a Windows-on-ARM machine that
is running the emulated x86_64 build replaces it in a normal major upgrade.

There is no portable zip in a release — the MSI is the Windows artifact. (The bundle step
still produces `dist\nullgate-windows-<arch>.zip` as a byproduct, handy for local testing.)

### Why ARM64 needs a native build at all
Windows on ARM runs x86_64 user-mode code under emulation, so the x86_64 MSI installs and the
app *looks* fine. But `wintun.dll` is backed by a **kernel driver**, and an ARM64 kernel will
not load an x64 driver — nor can an emulated x64 process load the ARM64 `wintun.dll` instead.
So under emulation the app runs and the routing silently doesn't. That is the whole reason the
ARM64 build exists, and it is why the updater must never fall back across architectures.

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
# The NullgateDaemon service can keep running — see Gotchas.
az login                                             # authenticate the signing session
pwsh -File scripts\build-msi.ps1                     # -> nullgate-<ver>-windows-x86_64.msi
pwsh -File scripts\build-msi.ps1 -Arch arm64         # -> nullgate-<ver>-windows-arm64.msi
signtool verify /pa target\wix\nullgate-<ver>-windows-x86_64.msi   # optional check
```
`build-msi.ps1` runs: release build → **sign the exes** → GTK bundle
(`bundle-gtk-windows.ps1`, which also copies the updater into `bin\`) → `wix build`
(`wix\nullgate.wxs`) → **sign the MSI**. Version is read from `Cargo.toml`. `-SkipBuild` packages
the already-built binaries; `-Version <x>` overrides the version.

One `.wxs` builds both architectures. The only thing in it that isn't architecture-neutral is
the WiX Util extension's custom-action binary, which ships one DLL per architecture — hence the
`-d UtilCA=` (`Wix4UtilCA_X64` / `Wix4UtilCA_A64`) the script passes.

## ARM64 (cross-built from x86_64)
The ARM64 build is split across **two ABIs**, which looks odd and isn't negotiable:

| | target | why |
|---|---|---|
| daemon, CLI | `aarch64-pc-windows-msvc` | no GTK dependency at all — they just cross-compile |
| GUI | `aarch64-pc-windows-gnullvm` | must match the ABI of the only ARM64 GTK that exists |

**Where ARM64 GTK comes from.** gvsbuild — the source of our x86_64 `C:\gtk` — is x64-only, and
vcpkg's `gtk` port explicitly excludes the platform (`"supports": "… & !(arm64 & windows)"`).
The only prebuilt GTK4 + libadwaita for Windows on ARM is **MSYS2's CLANGARM64** repo, and those
are mingw-ABI, which forces the GUI onto the `gnullvm` target. The mixed ABI costs nothing: the
GUI and the daemon are separate processes that only ever meet over a named pipe, so no ABI
boundary is ever crossed inside a process.

`scripts\fetch-gtk-msys2.ps1` resolves the dependency closure straight from the MSYS2 package
database and unpacks the `.pkg.tar.zst` archives — no MSYS2 or pacman install required, which is
what keeps the whole thing cross-buildable here.

One-time setup:
```powershell
rustup target add aarch64-pc-windows-msvc aarch64-pc-windows-gnullvm
# VS installer: "MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools" + the ARM64 Windows SDK
# llvm-mingw (ucrt-x86_64) from https://github.com/mstorsjo/llvm-mingw/releases -> C:\llvm-mingw-*
pwsh -File scripts\fetch-gtk-msys2.ps1                                  # -> C:\gtk-arm64
pwsh -File scripts\fetch-gtk-msys2.ps1 -Env ucrt64 -Root C:\gtk-msys2-x64
```
That second fetch is the **host-tools mirror**, and it is not optional. The GTK helper tools in
the ARM64 tree (`glib-compile-schemas`, `gdk-pixbuf-query-loaders`, `gtk4-update-icon-cache`) are
ARM64 binaries and cannot run on the build host — and `gdk-pixbuf-query-loaders` in particular
`dlopen()`s each loader, so it can only ever query loaders of its own architecture. Their output
is architecture-independent, so we run the **x86_64 build of the very same MSYS2 packages** to
generate it. `bundle-gtk-windows.ps1` asserts that the two trees' loader sets match before
trusting the cache it generates.

Because the bundle can't simply be launched here to see if it works, `scripts\verify-bundle.ps1`
reads the PE headers of every binary in it and checks that (a) they are all the expected
architecture and (b) every imported DLL is either bundled or a system DLL. It runs automatically
at the end of every bundle, for both architectures.

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
`packaging\windows\nullgate-update.ps1` is installed to `C:\Program Files\Nullgate\bin` and registered
as the SYSTEM scheduled task **`NullgateUpdate`** (daily ~3am ±2h, plus 5 min after boot). It compares
`nullgate-daemon.exe --version` to the latest release tag of the public
`steeb-k/nullgate` repo and, if newer, downloads the MSI and applies it silently
(`msiexec /i … /qn`). The MSI's `MajorUpgrade` stops the service, swaps files, and restarts it.
Logs: `%ProgramData%\nullgate\update.log`.

**Which MSI it picks.** The asset is chosen from the **OS** architecture
(`RuntimeInformation.OSArchitecture`, which reports the real OS even from an emulated process —
`PROCESSOR_ARCHITECTURE` does not), *not* from the installed build's. So a Windows-on-ARM machine
running the emulated x86_64 build migrates itself to the native ARM64 build on the next update,
which is exactly what we want (see "Why ARM64 needs a native build"). If a release has no MSI for
the machine's architecture the updater **stays put and does nothing** — it deliberately does not
fall back to another architecture's MSI, because on ARM64 that would install a build whose routing
silently cannot work.

**Restarting the tray agent.** The task runs as SYSTEM (session 0), but the tray agent runs in the
logged-in user's interactive session — the MSI's Restart Manager can close it but can't relaunch
it across that boundary. So the updater **closes any running `nullgate.exe` before the MSI** (the
tray agent and/or an open GUI window — both are `nullgate.exe` — so it replaces in place, no pending
reboot) and **relaunches the tray agent (`--agent`) in the user's session afterward** via a one-shot
Interactive scheduled task (`NullgateGuiRelaunch`, non-elevated); the user reopens the GUI window on
demand. On Linux/macOS the agent (and any open GUI) self-relaunch on the daemon's version change
instead (`ipn-gui` `relaunch_agent` / `restart_self`), where updater and agent share the session.

## Gotchas
- **The `NullgateDaemon` service does not need to be stopped for a release build.** The installed
  service runs the exe from `Program Files`, so it never holds a lock on `target\release\
  nullgate-daemon.exe`. (Only a daemon you launched *from the build tree* — `cargo run -p
  ipn-daemon` — would, and that one you'd stop anyway.)
- The MSI is **x64** (`-arch x64`) / **arm64** (`-arch arm64`) so it installs under `Program Files`,
  not `Program Files (x86)`. `ProgramFiles64Folder` and `System64Folder` both resolve to the
  *native* directories on ARM64, which is what the native service wants.
- The `UpgradeCode` in `wix\nullgate.wxs` is fixed **and shared by both architectures**; never
  change it or upgrades break. Sharing it is what makes an emulated-x64 → native-ARM64 migration a
  plain major upgrade rather than two products installed side by side.
- **`winresource` emits an x64 resource object when cross-compiling to ARM64.** It deliberately
  passes no `--target` to `windres` for aarch64, so an unprefixed `windres` defaults to x86-64 and
  the link dies with `machine type x64 conflicts with arm64`. `crates\ipn-gui\build.rs` pins
  `aarch64-w64-mingw32-windres` when the *target* is aarch64; don't "simplify" that away.
- **`ring` needs `clang` on `PATH`** for either aarch64 target (it builds aarch64 assembly). It is
  the only dependency in the tree that does.
- **Don't copy `*.dll` wholesale out of the MSYS2 tree.** Unlike gvsbuild's purpose-built GTK
  prefix, MSYS2's `bin\` is a shared prefix for every package in the closure — a blanket copy ships
  `libpython3.14.dll` and friends. The bundler walks the import tables from our binaries (plus the
  pixbuf loaders, which GTK `dlopen`s rather than imports) and copies only that closure.
