# Packaging & releasing

From 0.1.0, Nullgate ships **real installers** with **auto-update**: a code-signed **Windows MSI**, a
**Linux tarball** (system-service installer), and a **macOS** universal `.app` tarball. Releases
are published to the **public `steeb-k/nullgate` repo**; the in-product updaters and
the `install.sh` one-liner read its `releases/latest`. Builds are **local, no CI** — each OS's
artifact is built on that OS (Windows native; Linux via WSL; macOS on a Mac).

Per-platform detail: [windows-packaging.md](windows-packaging.md),
[linux-packaging.md](linux-packaging.md), [macos-packaging.md](macos-packaging.md).

## Versioning
- The workspace version lives once in the root `Cargo.toml` `[workspace.package]`; all crates
  inherit it via `version.workspace = true`. The exe `--version` (and the updaters' comparison)
  come from this.
- Releases are tagged `v<version>` (e.g. `v0.1.0`). The release marked **Latest** on GitHub is
  what every updater fetches — publish the newest one last and don't leave it as a draft.
- (Pre-0.1.0 we used throwaway `v0.0.1-test<N>` prereleases; that scheme is retired.)

## Release checklist
1. **Tests:** `cargo test -p ipn-core` and the relevant ignored e2e tests pass; `cargo build
   --workspace` is clean on Windows and Linux.
2. **Bump** the version in root `Cargo.toml`, run `cargo update --workspace`, and move
   `CHANGELOG.md`'s `## [Unreleased]` items under a `## [<version>]` heading. **Also bump the
   Android version by hand** in `android/app/build.gradle.kts` — `versionName` (to match the
   workspace version) and `versionCode` (`MAJOR*10000 + MINOR*100 + PATCH`); these are hardcoded
   literals, **not** derived from `Cargo.toml`, so they're easy to forget and a stale `versionCode`
   silently blocks in-place Android updates. Commit.
3. **Build each artifact on its own OS:**
   - **Windows** (signed): `az login`, then
     `pwsh -File scripts\build-msi.ps1` → `target\wix\nullgate-<ver>-windows-x86_64.msi`, and
     `pwsh -File scripts\build-msi.ps1 -Arch arm64` → `…-windows-arm64.msi`. Both are built on
     the x86_64 box (ARM64 is fully cross-compiled); see `windows-packaging.md` for its one-time
     toolchain setup. **Ship both or neither** — the updater picks its asset by OS architecture
     and will not fall back across architectures, so a release with only the x86_64 MSI silently
     strands every ARM64 install on its current version.
   - **Linux** (WSL/Linux): `scripts/package-linux.sh` → `dist/nullgate-<ver>-linux-x86_64.tar.gz`.
   - **macOS** (on a Mac): once, create the conda-forge GTK env(s) with
     `scripts/setup-conda-macos.sh --universal` (needs `micromamba`/`mamba`/`conda` on PATH);
     then `scripts/package-macos.sh` → `dist/nullgate-<ver>-macos-{universal|arm64}.tar.gz`.
     Verify the floor: `otool -l …/Contents/lib/libgtk-4.*.dylib | grep -A3 LC_BUILD_VERSION`
     shows `minos 11.0`. See `macos-packaging.md` (conda-forge GTK, not Homebrew).
   - **Android** (signed APK): needs `android/keystore.properties` (the **stable** release
     keystore — see the Android note below). `cd android && ./gradlew :app:assembleRelease`
     produces `android/app/build/outputs/apk/release/app-release.apk`; rename it to
     `nullgate-<ver>-android.apk` for upload. Confirm you bumped `versionCode`/`versionName` in
     step 2 first — a stale `versionCode` silently blocks in-place updates (Obtainium included).
4. **Publish** to the public repo (authenticated `gh`). Create the release with whatever's ready,
   then upload the rest as each OS finishes:
   ```sh
   gh release create v<ver> --repo steeb-k/nullgate \
     --title "v<ver>" --notes-file release-notes.md \
     target/wix/nullgate-<ver>-windows-x86_64.msi \
     target/wix/nullgate-<ver>-windows-arm64.msi
   gh release upload v<ver> --repo steeb-k/nullgate dist/nullgate-<ver>-linux-x86_64.tar.gz
   gh release upload v<ver> --repo steeb-k/nullgate dist/nullgate-<ver>-macos-universal.tar.gz
   gh release upload v<ver> --repo steeb-k/nullgate nullgate-<ver>-android.apk
   ```
   Asset names must stay `nullgate-<ver>-<platform>.<ext>` — the desktop updaters glob on
   `windows-x86_64.msi`, `windows-arm64.msi`, `linux-x86_64.tar.gz`, and
   `macos-(universal|<arch>).tar.gz`; the Android build is a single universal
   `nullgate-<ver>-android.apk` (Obtainium auto-selects the lone `.apk` asset — see the Android
   note below).

## Smoke-check before announcing
- **Windows:** install the MSI on a clean machine; confirm the app opens, the `NullgateDaemon` service
  runs, and the `NullgateUpdate` task exists (`schtasks /Query /TN NullgateUpdate`).
- **Linux/macOS:** run the `curl … | sh` one-liner; confirm `nullgatectl --status` shows the daemon
  active and the updater enabled.
- **Two machines:** create on one, join on the other, compare the emoji code, approve, connect
  RDP/SSH to the peer's `10.99.0.x`.
- **Auto-update path:** with an older build installed, publish a newer release and confirm the
  updater picks it up (or force it: Windows `…\bin\ipn-update.ps1 -Check`; Linux/macOS
  `nullgatectl --update --check`).
- **Android:** on a device that already has the *previous* release-signed build,
  `adb install -r nullgate-<ver>-android.apk` — it must update **in place** (no uninstall) and
  launch. An `INSTALL_FAILED_UPDATE_INCOMPATIBLE`/signature clash means the keystore changed since
  the installed build; stop and fix it before publishing (see the Android note below).

## Notes
- The signing metadata (`artifact-signing-metadata.json`) is **git-ignored** — see
  [windows-packaging.md](windows-packaging.md). Never commit it or the generated `wix/license.rtf`.

## Android APK & Obtainium
Each release also carries one **signed universal APK**, `nullgate-<ver>-android.apk` (build +
naming in step 3). There's no in-product Android updater; users update either manually or through
**[Obtainium](https://github.com/ImranR98/Obtainium)**, which the release structure already
supports out of the box:

- **Add the app in Obtainium** with the source URL `https://github.com/steeb-k/nullgate`. Obtainium
  tracks the release marked **Latest**, auto-selects the lone `.apk` asset (the MSI/tarballs are
  ignored — no APK filter needed), and offers an update whenever the APK's `versionCode` climbs.
  Nothing extra to publish; keep doing the normal release.
- **Signature stability is mandatory.** Android — and every sideload updater, Obtainium included —
  refuses to install an update signed with a different key than the installed app. So **every**
  release APK must be signed with the *same* `android/keystore.properties` keystore. A lost or
  rotated key permanently blocks in-place updates (users would have to uninstall, losing that
  device's identity/secrets). Back the keystore up; never rotate it. See
  [android-packaging.md](android-packaging.md).
- **Don't mark an APK-bearing release as draft or pre-release** — Obtainium skips those by default,
  so it would never see the update (same "publish Latest last" rule as the desktop updaters).
- **First-install caveat:** a phone that currently has a **debug-signed** build (from
  `run-android.ps1` without `-Release`) can't be updated by the release-signed APK — the keys
  differ. It must uninstall the debug build once, then install the release APK; after that every
  Obtainium update is in-place.
