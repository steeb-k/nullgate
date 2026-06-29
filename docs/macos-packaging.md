# macOS packaging (self-contained .app + LaunchDaemon + auto-updater)

How the macOS release is built and installed. Must be built **on a Mac** (the GTK dylib closure
is bundled from Homebrew; it can't be cross-built from Windows/Linux).

## What ships
`nullgate-<version>-macos-<arch>.tar.gz` (`arch` = `universal` or `arm64`) containing a self-contained,
ad-hoc-signed **"Nullgate.app"** (bundled GTK), the `nullgatectl` manager, and the launchd
templates. Because Nullgate's daemon needs root to create the `utun` interface, install sets it up as
a **root LaunchDaemon** (not a per-user agent); the tray GUI runs per-user.

## Prerequisites
- **Xcode Command Line Tools**: `xcode-select --install` (`install_name_tool`, `codesign`,
  `otool`, `lipo`, `iconutil`, `sips`).
- **Rust**: `rustup default stable`. For universal2 also `rustup target add x86_64-apple-darwin`.
- **Homebrew GTK** (arm64): `brew install gtk4 libadwaita pkg-config` (at `/opt/homebrew`).
- For **universal2** only: a second **x86_64 Homebrew** at `/usr/local` (under Rosetta) with
  `gtk4 libadwaita pkg-config` installed:
  ```sh
  softwareupdate --install-rosetta --agree-to-license
  arch -x86_64 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  arch -x86_64 /usr/local/bin/brew install gtk4 libadwaita pkg-config
  ```

## Build
```sh
scripts/package-macos.sh                 # cargo build --release, bundle, package
scripts/package-macos.sh --skip-build    # repackage existing per-arch release bins
# -> dist/nullgate-<version>-macos-{universal|arm64}.tar.gz
```
It builds the arm64 `.app`, bundles the GTK closure (`scripts/bundle-gtk-macos.sh`: walks the
`otool` closure of `nullgate`, copies non-system dylibs into `Contents/lib`, rewrites install names to
`@executable_path/../lib/<name>`, ad-hoc re-signs inside-out, regenerates the pixbuf
`loaders.cache`, compiles GSettings schemas, bundles fontconfig). With both Homebrew arches
present it also builds an x86_64 `.app` and `lipo`s every Mach-O into a universal one. Then it
writes `Info.plist`, renders `AppIcon.icns`, and seals the bundle. **Build on the oldest macOS
you want to support** (the bundled GTK's `minos` sets the floor).

## Install / manage (on the target)
```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/iroh-private-network/main/install.sh | sh
```
Or from the tarball: `./nullgatectl --install`. `nullgatectl` uses `sudo` and:
- copies the app to `/Applications/Nullgate.app`,
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
`ipn-daemon --version` to the latest tag of the public `steeb-k/iroh-private-network` repo,
downloads the matching tarball, swaps the `.app`, and reloads the daemon.
