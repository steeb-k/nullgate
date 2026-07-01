# Packaging & releasing

From 0.1.0, Nullgate ships **real installers** with **auto-update**: a code-signed **Windows MSI**, a
**Linux tarball** (system-service installer), and a **macOS** universal `.app` tarball. Releases
are published to the **public `steeb-k/iroh-private-network` repo**; the in-product updaters and
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
   `CHANGELOG.md`'s `## [Unreleased]` items under a `## [<version>]` heading. Commit.
3. **Build each artifact on its own OS:**
   - **Windows** (signed): stop the service, `az login`, then
     `pwsh -File scripts\build-msi.ps1` → `target\wix\nullgate-<ver>-windows-x86_64.msi`.
   - **Linux** (WSL/Linux): `scripts/package-linux.sh` → `dist/nullgate-<ver>-linux-x86_64.tar.gz`.
   - **macOS** (on a Mac): once, create the conda-forge GTK env(s) with
     `scripts/setup-conda-macos.sh --universal` (needs `micromamba`/`mamba`/`conda` on PATH);
     then `scripts/package-macos.sh` → `dist/nullgate-<ver>-macos-{universal|arm64}.tar.gz`.
     Verify the floor: `otool -l …/Contents/lib/libgtk-4.*.dylib | grep -A3 LC_BUILD_VERSION`
     shows `minos 11.0`. See `macos-packaging.md` (conda-forge GTK, not Homebrew).
4. **Publish** to the public repo (authenticated `gh`). Create the release with whatever's ready,
   then upload the rest as each OS finishes:
   ```sh
   gh release create v<ver> --repo steeb-k/iroh-private-network \
     --title "v<ver>" --notes-file release-notes.md \
     target/wix/nullgate-<ver>-windows-x86_64.msi
   gh release upload v<ver> --repo steeb-k/iroh-private-network dist/nullgate-<ver>-linux-x86_64.tar.gz
   gh release upload v<ver> --repo steeb-k/iroh-private-network dist/nullgate-<ver>-macos-universal.tar.gz
   ```
   Asset names must stay `nullgate-<ver>-<platform>.<ext>` — the updaters glob on
   `windows-x86_64.msi`, `linux-x86_64.tar.gz`, and `macos-(universal|<arch>).tar.gz`.

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

## Notes
- The signing metadata (`artifact-signing-metadata.json`) is **git-ignored** — see
  [windows-packaging.md](windows-packaging.md). Never commit it or the generated `wix/license.rtf`.
- Android (signed APK via UniFFI + `VpnService`) is still planned; see `TODO.md`.
