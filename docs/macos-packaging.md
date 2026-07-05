# macOS packaging (self-contained .app + LaunchDaemon + auto-updater)

How the macOS release is built and installed. Must be built **on a Mac** (the GTK dylib closure
is bundled from a local prefix; it can't be cross-built from Windows/Linux).

## What ships
`nullgate-<version>-macos-<arch>.tar.gz` (`arch` = `universal` or `arm64`) containing a self-contained,
ad-hoc-signed **"Nullgate.app"** (bundled GTK), the `nullgatectl` manager, and the launchd
templates. Because Nullgate's daemon needs root to create the `utun` interface, install sets it up as
a **root LaunchDaemon** (not a per-user agent); the tray agent runs per-user and opens the GUI on demand.

## Why conda-forge GTK, not Homebrew
The GTK dylibs we bundle set the app's minimum-macOS floor (their `minos` load command). Homebrew
stamps the **build host's** OS, so an arm64 gtk4 built on a modern dev box carries a very high
`minos` — e.g. on macOS 26 it's `minos 26.0`, which refuses to launch on any older Mac. conda-forge
instead builds `osx-arm64` against the **macOS 11** SDK (the floor for all Apple Silicon) and
`osx-64` against ~10.13, so the bundled dylibs carry `minos 11.0` no matter what this machine runs.

| GTK source | arm64 `minos` | x86_64 `minos` | resulting floor |
|---|---|---|---|
| conda-forge (**what we ship**) | 11.0 | ~10.13 | **macOS 11** |
| Homebrew on a macOS 26 box | 26.0 | 14.x | macOS 26 (broken for testers) |

## Prerequisites
- **Xcode Command Line Tools**: `xcode-select --install` (`install_name_tool`, `codesign`,
  `otool`, `lipo`, `iconutil`, `sips`).
- **Rust**: `rustup default stable`. For universal also `rustup target add x86_64-apple-darwin`.
- **A conda front-end on PATH** — `mamba`, `micromamba`, or `conda`. The lightweight option is a
  standalone [`micromamba`](https://mamba.readthedocs.io/en/latest/installation/micromamba-installation.html)
  binary; put its directory on `PATH` and export `MAMBA_ROOT_PREFIX` to a scratch dir. (miniforge
  also works.) No Homebrew GTK is needed.
- **conda-forge GTK env(s)** — created once by `scripts/setup-conda-macos.sh`:
  ```sh
  scripts/setup-conda-macos.sh              # osx-arm64 env only (arm64 build)
  scripts/setup-conda-macos.sh --universal  # + osx-64 env (universal arm64 + Intel)
  ```
  This writes `.conda-gtk/{arm64,x86}` (git-ignored) with `gtk4 libadwaita librsvg pkg-config` and
  the `.pc`-only helpers the Rust `-sys` builds need. Override the paths via
  `NULLGATE_CONDA_ARM` / `NULLGATE_CONDA_X86`.

## Build
```sh
scripts/setup-conda-macos.sh --universal # once: create the conda-forge GTK env(s)
scripts/package-macos.sh                 # cargo build --release, bundle, package
scripts/package-macos.sh --skip-build    # repackage existing per-arch release bins
# -> dist/nullgate-<version>-macos-{universal|arm64}.tar.gz
```
`package-macos.sh` builds each slice with `pkg-config` pointed at the conda env
(`PKG_CONFIG_LIBDIR=.conda-gtk/<arch>/lib/pkgconfig`), `MACOSX_DEPLOYMENT_TARGET=11.0`, and
`-headerpad_max_install_names` (conda's short `@rpath` install names grow when relocated, so the
Mach-O header needs slack). It bundles the GTK closure (`scripts/bundle-gtk-macos.sh`: walks the
`otool` closure of `nullgate`, copies non-system dylibs into `Contents/lib`, rewrites install names to
`@executable_path/../lib/<name>`, ad-hoc re-signs inside-out, regenerates the pixbuf
`loaders.cache`, compiles GSettings schemas, bundles fontconfig). When the `osx-64` env **and** the
`x86_64-apple-darwin` Rust target are present it also builds an x86_64 `.app` and `lipo`s every
Mach-O into a universal one (re-signing after, since `lipo` invalidates the ad-hoc signature).
Then it writes `Info.plist`, renders `AppIcon.icns`, and seals the bundle. At runtime the GUI's
`setup_runtime_env()` points GTK at the bundled `share/`+`lib/`+`etc/` relative to the executable.

## Verify the OS floor
Confirm the bundled GTK carries the macOS-11 floor (not the build host's OS):
```sh
otool -l "dist/nullgate-<ver>-macos-universal/Nullgate.app/Contents/lib/libgtk-4."*.dylib \
  | grep -A3 LC_BUILD_VERSION      # expect: minos 11.0 (arm64 slice)
lipo -archs "dist/nullgate-<ver>-macos-universal/Nullgate.app/Contents/MacOS/nullgate"  # -> arm64 x86_64
```

## Install / manage (on the target)
```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/nullgate/main/install.sh | sh
```
Or from the tarball: `./nullgatectl --install`. `nullgatectl` uses `sudo` and:
- copies the app to `/Applications/Nullgate.app` (via `ditto`), then registers it with Launch
  Services (`lsregister -f`) and imports it into Spotlight (`mdimport`) — a plain copy into
  `/Applications` skips the registration a Finder drag does, leaving the app absent from Spotlight
  and "Open With". This runs on every `--update` too, since replacing the bundle can stale the record,
- symlinks `nullgate`/`ipn-daemon`/`ipn-cli` into `/usr/local/bin` (and installs `nullgatectl` there),
- installs `/Library/LaunchDaemons/io.github.steeb_k.Nullgate.daemon.plist` (root; owns the utun) and
  `…Nullgate.update.plist` (root; daily auto-update), and bootstraps them into the `system` domain,
- installs `/Library/LaunchAgents/io.github.steeb_k.Nullgate.gui.plist` (per-user tray autostart).

Manage: `nullgatectl --status`, `nullgatectl --update [--check]`, `nullgatectl --uninstall [--purge]`.

## Signing / notarization
The app is **ad-hoc signed only** (no Developer ID, no notarization). Installing via the
`curl … | sh` command or the tarball does **not** set the `com.apple.quarantine` xattr, so
Gatekeeper doesn't block it. (Distributing the raw `.app` via a browser download *would* be
quarantined — use the installer command.)

## Auto-update
`…Nullgate.update.plist` (root LaunchDaemon; daily at 13:00 + at load) runs `nullgatectl --update`: compares
`ipn-daemon --version` to the latest tag of the public `steeb-k/nullgate` repo,
downloads the matching tarball, swaps the `.app`, and reloads the daemon.
