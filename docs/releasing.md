# Packaging & releasing

From 0.1.0, Nullgate ships **real installers** with **auto-update**: code-signed **Windows MSIs**
(x86_64 + ARM64), a **Linux tarball** (system-service installer), a **macOS** universal `.app`
tarball (Developer-ID signed and notarized), and a signed **Android APK**. Releases are published to
the **public `steeb-k/nullgate` repo**; the in-product updaters and the `install.sh` one-liner read
its `releases/latest`.

**Since 0.7.1 the artifacts are built and published by CI from a tag** —
[ci-release.md](ci-release.md). The per-platform scripts below still work by hand and are what CI
runs; building locally is the fallback, not the process.

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
1. **Tests:** `cargo test -p ipn-core` and the relevant ignored e2e tests pass (CI's `gate` job
   repeats the unit tests and cargo-deny, not the e2e ones).
2. **Bump** the version in root `Cargo.toml`, run `cargo update --workspace`, and move
   `CHANGELOG.md`'s `## [Unreleased]` items under a `## [<version>] - <date>` heading (the release
   notes are cut from that section). **Also bump the Android version by hand** in
   `android/app/build.gradle.kts` — `versionName` (to match) and `versionCode`
   (`MAJOR*10000 + MINOR*100 + PATCH`); the `publish` job refuses a tag where these disagree, so
   forgetting is loud rather than silent. Commit and push.
3. **Rehearse if anything in the pipeline changed:** `git tag v<ver>-test1 && git push --tags`.
   That publishes a prerelease with all five assets, which no updater or Obtainium will take.
   Install one or two of them by hand (smoke-check below), then delete the prerelease and the tag.
4. **Tag:** `git tag v<ver> && git push origin v<ver>`. The `release` workflow builds all four
   platforms, gates on the checks, and creates the release with every asset in one call. If the
   `release` environment has a required reviewer, approve it in the Actions tab.
5. If a platform job fails, nothing is published. Fix on `main`, then dispatch `release.yml` with
   `tag: v<ver>` — the fixed workflow builds the tag's source.

### Building by hand (fallback)
Each artifact on its own OS: **Windows** `pwsh -File scripts\build-msi.ps1` (+ `-Arch arm64`;
signed if `artifact-signing-metadata.json` + `az login` are present — see `windows-packaging.md`);
**Linux** `scripts/package-linux.sh`; **macOS** `scripts/setup-conda-macos.sh --universal` once,
then `CODESIGN_IDENTITY='Developer ID Application: …' NULLGATE_NOTARIZE=1 scripts/package-macos.sh`
(ad-hoc without the identity — installable via `nullgatectl`, not by browser download);
**Android** `cd android && ./gradlew :app:assembleRelease` with `android/keystore.properties`,
renamed to `nullgate-<ver>-android.apk`. Publish with `gh release create v<ver> --verify-tag
--latest` and **all five files in the same command** — asset names must stay
`nullgate-<ver>-<platform>.<ext>` (`windows-x86_64.msi`, `windows-arm64.msi`, `linux-x86_64.tar.gz`,
`macos-universal.tar.gz`, `android.apk`).

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
