# CI release pipeline — plan

Status: **planned, not built.** Until the pipeline below has produced a rehearsal release that
every updater ignores (a `-testN` prerelease) *and* a real one that every updater takes, the rule in
`CLAUDE.md` stands: releases are built locally. This document is the course; it replaces the
"builds are local, no CI" paragraph of `releasing.md` when it lands.

## Why
Every release is four artifacts built on three operating systems by hand. The macOS build alone
means a Mac, a conda env, and an afternoon. The pipeline moves *building* to GitHub Actions while
keeping the two things that must not change: **the assets the updaters look for**, and **the
signatures Windows and Android verify**.

## What must not change (the updater contract)
The in-product updaters and `install.sh` read `releases/latest` on `steeb-k/nullgate` and glob
asset names. That contract is fixed and the pipeline must reproduce it exactly:

| Asset | Consumer | Rule |
|-------|----------|------|
| `nullgate-<ver>-windows-x86_64.msi` + `…-windows-arm64.msi` | `nullgate-update.ps1` picks by OS arch, never falls back | **both or neither** |
| `nullgate-<ver>-linux-x86_64.tar.gz` | `nullgatectl --update` | built on the oldest glibc we accept (see Linux) |
| `nullgate-<ver>-macos-universal.tar.gz` | `nullgatectl`/`install.sh` accept `universal` or `<host arch>` | one universal tarball serves Intel **and** Apple Silicon |
| `nullgate-<ver>-android.apk` | Obtainium takes the lone `.apk` | same keystore, `versionCode` bumped |

Two properties follow: the release is **published in one shot with every asset attached** (never
create-then-upload — a `Latest` release missing an arch strands that arch), and it is **never a
draft or prerelease** (both are invisible to every updater). Conversely a tag like
`v0.7.1-test1` is published *as* a prerelease on purpose: it exercises the whole pipeline and no
installed device will ever see it.

## Shape
One workflow, `release.yml`, triggered by pushing a `v*` tag (and `workflow_dispatch` for
rehearsals). Five build jobs run in parallel; a sixth publishes only if all five succeeded.

```
tag v0.7.1 ─┬─ check      ubuntu   cargo-deny + cargo test (the two existing workflows, reused)
            ├─ linux      ubuntu-24.04     package-linux.sh            → linux-x86_64.tar.gz
            ├─ macos      macos-15 (arm64) package-macos.sh (universal) → macos-universal.tar.gz
            ├─ android    ubuntu-24.04     gradlew :app:assembleRelease → android.apk
            ├─ windows    windows-2025     build-msi.ps1 ×2 (x86_64, arm64) → two .msi, signed
            └─ publish    ubuntu, environment "release" (manual approval)
                          verifies tag == Cargo.toml == android versionName,
                          `gh release create v<ver> <all five assets>` in one call
```

Guardrails:
- **Version gate.** `publish` refuses unless the tag, the workspace `version`, and Android's
  `versionName`/`versionCode` agree. This is the step people forget by hand (`releasing.md` step 2).
- **All-or-nothing.** `publish` needs all five artifacts; a red job means no release, not a partial
  one.
- **Environment `release` with a required reviewer** (you). Signing credentials and the Android
  keystore are scoped to that environment, so they are only ever exposed to a run you approved,
  from a tag on `main`.
- **Pinned action SHAs, `--locked`, cargo-deny first** — same posture as the existing check
  workflows.

## Per-platform notes (what each job needs and where the risk is)

**Linux** — `ubuntu-24.04` (GTK 4.14 satisfies the `v4_10` feature; 22.04 ships GTK 4.6 and can't
build the GUI). The tarball does not bundle GTK, but it does bind the target's **glibc** to the
runner's (2.39). Today's WSL builds set that floor implicitly too; the plan makes it explicit and
documented. Needs `libgtk-4-dev libadwaita-1-dev libdbus-1-dev imagemagick`. No secrets.

**macOS** — `macos-15` (Apple Silicon). `setup-conda-macos.sh --universal` with `micromamba`
(cache `.conda-gtk/` keyed on the script + the conda-forge pins, or the env creation is ~10 min per
run), `rustup target add x86_64-apple-darwin`, then `package-macos.sh`, which already lipo's a
universal `.app` when the osx-64 env is present. The app is **ad-hoc signed** today, not
Developer-ID signed or notarized, and installs go through `nullgatectl`/`curl | sh` (no
quarantine), so this needs **no Apple secrets**. That is the single biggest win of the pipeline:
no more Mac in the loop. (If notarization is ever wanted, it's an additive step: a Developer ID
cert in the `release` environment + `notarytool`.)

**Android** — `ubuntu-24.04`, JDK 17, SDK 35, NDK r27c, `cargo-ndk`, the three Android Rust
targets; `gradlew :app:assembleRelease` runs cargo-ndk itself. Secrets: the release keystore
(base64) and `keystore.properties`, both written to `android/` only inside the job. The keystore is
the one credential that can **never** be rotated (a different key blocks in-place updates for every
user), so it goes in the protected environment and nowhere else.

**Windows** — `windows-2025`, the long pole. Pieces:
- GTK x64: gvsbuild publishes prebuilt zips (`GTK4_Gvsbuild_<ver>_x64.zip` on
  `wingtk/gvsbuild` releases) — download by pinned version + checksum into `C:\gtk` instead of
  building GTK from source (~1 h). Pin the same version the local `C:\gtk` was built with, or bump
  deliberately and re-test.
- ARM64: `fetch-gtk-msys2.ps1` (already scripted, downloads MSYS2's CLANGARM64 GTK), llvm-mingw
  (release zip, pinned), LLVM/clang (on the runner image), MSVC ARM64 build tools (on the image),
  `rustup target add aarch64-pc-windows-msvc aarch64-pc-windows-gnullvm`; then
  `build-arm64.ps1` and `verify-bundle.ps1` prove the bundle is single-arch.
- WiX 5 as a dotnet tool (already what `build-msi.ps1` expects).
- **Signing: Azure Trusted Signing via OIDC.** `sign-artifacts.ps1` already authenticates through
  the `az` CLI session, so `azure/login` with a **federated credential** (no client secret on
  GitHub) on an app registration holding the *Trusted Signing Certificate Profile Signer* role is a
  drop-in; the git-ignored `artifact-signing-metadata.json` becomes an environment secret written
  to disk for the job. The Trusted Signing client tools + Windows SDK `signtool` are installed in
  the job (both are downloadable; `sign-artifacts.ps1` finds them via `$env:SIGNTOOL_PATH` /
  `$env:ARTIFACT_SIGNING_DLIB`). Unsigned MSIs are a hard failure in CI, not a warning — the
  script's "skip if metadata absent" leniency is for dev boxes.

## Rollout
1. **Phase 1 — build only** (`workflow_dispatch`, artifacts uploaded to the run, nothing
   published): linux, macos, android. Install each artifact on a real machine by hand
   (`releasing.md` smoke-check). Fix drift between the runner and the WSL/Mac builds here.
2. **Phase 2 — Windows**: same, plus signed x86_64 + arm64 MSIs; SmartScreen-clean install on a
   clean VM; `verify-bundle.ps1` green.
3. **Phase 3 — publish**: the `release` environment; tag `v0.7.1-test1`, approve, confirm a
   prerelease with all five assets appears and that an installed 0.7.0 updater does **not** take it.
4. **Cut `v0.7.1`** through the pipeline; confirm each updater takes it (Windows scheduled task,
   `nullgatectl --update --check`, Obtainium). Then rewrite `releasing.md`'s checklist around
   "push a tag, approve", move this file's content into it, and change the `CLAUDE.md` rule from
   "never build in Actions" to "only `release.yml` ships, only from a tag, only after approval".

## Decisions to confirm
- **Universal macOS tarball vs. separate arm64 + x86_64.** Universal is one job, one asset, and
  what both updaters already accept; separate slices halve the download but need an Intel runner
  (`macos-15-intel`) too. Recommendation: universal.
- **OIDC vs. a client secret for Azure.** OIDC needs a one-time federated-credential setup on the
  app registration; a client secret is faster to wire and worse to hold. Recommendation: OIDC.
- **Manual approval on `publish`.** It is the one human step left; it also means a tag push alone
  can't publish a release from a compromised laptop. Recommendation: keep it.
