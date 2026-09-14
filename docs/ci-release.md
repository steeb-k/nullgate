# CI release pipeline

Pushing a `v<version>` tag builds every platform on GitHub Actions and publishes one release with
all five assets. The shape and most of the signing machinery are lifted from `steeb-k/commune`,
which has shipped this way since September 2026.

## The updater contract (what must never change)
The in-product updaters and `install.sh` read `releases/latest` on `steeb-k/nullgate` and glob
asset names, so the pipeline reproduces exactly:

| Asset | Consumer | Rule |
|-------|----------|------|
| `nullgate-<ver>-windows-x86_64.msi` + `…-windows-arm64.msi` | `nullgate-update.ps1`, picks by OS arch, never falls back | **both or neither**; Authenticode must be **Valid** or the updater refuses it |
| `nullgate-<ver>-linux-x86_64.tar.gz` | `nullgatectl --update` | built on `ubuntu-24.04`, i.e. a glibc 2.39 floor |
| `nullgate-<ver>-macos-universal.tar.gz` | `nullgatectl` accepts `universal` or `<host arch>` | one universal tarball serves Intel and Apple Silicon; Developer-ID signed, notarized, stapled; `nullgatectl` refuses a bundle whose Team ID differs from the installed one |
| `nullgate-<ver>-android.apk` | Obtainium takes the lone `.apk` | the **same** keystore every release; `versionCode` bumped |

Consequences the workflow enforces: the release is created in **one call with every asset**
(never create-then-upload — a `Latest` missing an arch strands that arch); it is never a draft;
`v<ver>-test<N>` tags publish a **prerelease**, which every updater and Obtainium ignore; and the
`publish` job refuses unless the tag, the workspace `version` and Android's
`versionName`/`versionCode` (`MAJOR*10000+MINOR*100+PATCH`) agree.

## Shape
`.github/workflows/release.yml` (tag push, or `workflow_dispatch`) →

```
gate     ubuntu-24.04    cargo build/test + cargo-deny (what ci.yml runs)
build    ./build.yml     four jobs in parallel:
  linux      ubuntu-24.04   scripts/package-linux.sh
  android    ubuntu-24.04   gradlew :app:assembleRelease, release keystore from secrets
  macos      macos-15       setup-conda-macos.sh --universal, package-macos.sh (signs with the
                            Developer ID from a throwaway keychain, notarizes, staples), checks
  windows    windows-2025   gvsbuild GTK zip (pinned + sha256) -> C:\gtk; MSYS2 CLANGARM64 via
                            fetch-gtk-msys2.ps1; llvm-mingw; both arches built, then
                            build-msi.ps1 -SkipBuild x2 (signs exes + MSIs via Azure Trusted
                            Signing over OIDC); verify-bundle.ps1; Authenticode must be Valid
publish  ubuntu-24.04    version gate, exactly five assets, notes from CHANGELOG.md,
                         `gh release create --verify-tag` in one shot (prerelease for -testN)
```

Dispatch inputs: `tag` (an existing tag to build and publish; empty = build the current ref and
publish nothing), `publish`, `sign`. A dispatched run takes the *workflow* from the branch it was
dispatched on and the *source* from the tag — the recovery path for a CI bug.

## Signing
**Windows — Azure Trusted Signing, no secret on GitHub.** `azure/login` exchanges a GitHub OIDC
token for an Azure CLI session; `scripts/sign-artifacts.ps1` already authenticates through the CLI,
so it is a drop-in. The account/profile names live in the committed
`scripts/artifact-signing-metadata.ci.json` (the git-ignored root copy is for a laptop's own
session). The Azure app registration needs a **federated credential** whose subject is
`repo:steeb-k/nullgate:environment:release` — hence `environment: release` on the Windows job — and
GitHub now presents the immutable-ID spelling (`repo:steeb-k@<userid>/nullgate@<repoid>:environment:release`),
so register **both** forms, exactly as commune does. The role is *Artifact Signing Certificate
Profile Signer* on the profile. Two logins per job: the first lets a two-second signing probe fail
fast, the second is fresh for signtool because the OIDC assertion lives five minutes and a stale one
fails inside `SignerSign()` with an unreadable `0x80004005`.

**macOS — Developer ID + notarization.** `scripts/ci/macos-keychain.sh` imports the `.p12` from
secrets into a throwaway keychain and exports `CODESIGN_IDENTITY`; `scripts/package-macos.sh` then
signs every Mach-O with `--timestamp --options runtime`, seals the bundle, verifies it, and (with
`NULLGATE_NOTARIZE=1`) runs `scripts/notarize-macos.sh`, which submits to the notary service,
prints the notary log on rejection, and staples the ticket to the `.app` **before** it is tarred
(a ticket stapled to a disk image does not travel inside a tarball). The certificate is a *second*
Developer ID Application certificate under the same team (the maintainer's own is Xcode's
cloud-managed kind and cannot be exported); the team is what `nullgatectl` compares, so that is
invisible to installed machines.

**Android** — the release keystore, base64 in a secret, written to `android/` for the job only.

## Secrets and setup (one-time)
Repository secrets on `steeb-k/nullgate` (same names as commune; the Azure and macOS values are
the same credentials, the Android keystore is Nullgate's own):

| Secret | Value |
|--------|-------|
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` | the signing app registration (as on commune) |
| `MACOS_CODESIGN_IDENTITY` | `Developer ID Application: <Name> (<TEAMID>)` |
| `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD` | base64 of the CI `.p12`, and its password |
| `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_PASSWORD`, `MACOS_NOTARY_TEAM_ID` | app-specific password form (or `MACOS_NOTARY_KEY`, `_KEY_ID`, `_ISSUER_ID` for an API key) |
| `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` | the keystore behind `android/keystore.properties` |

**Already done (2026-09-13):** the `release` environment exists (auto-created by the first run;
add yourself as a required reviewer if a release should wait for approval), and the Azure app
registration `commune-ci-signing` (client id `bbcfe359-662c-4b21-9828-b8642c55613a`, tenant
`249fb121-…`, subscription `f2204534-…`) carries both federated credentials for nullgate —
`github-nullgate-release-environment` and `…-ids` — beside commune's, so one app signs both
projects with the `skz-code` / `ddrx-pcsvc` profile. The signing material (Android keystore, macOS CI Developer ID `.p12` and notary password,
Windows metadata) is kept **outside the repository** in the maintainer's `~/nullgate-signing/`,
which also holds `set-nullgate-secrets.sh` — the one command that (re)sets every secret above
from those files without printing a value.

**Rehearsal 2026-09-13** (`workflow_dispatch`, unsigned, unpublished): gate, Linux, macOS universal
and Windows x86_64 + ARM64 all passed on hosted runners at the first attempt that reached them;
Android needed two fixes (r27c withdrawn from sdkmanager → the image's r27d; `gradlew` lacked its
exec bit in git).

## Rollout
1. `gh workflow run release.yml -f publish=false -f sign=false` — every platform builds, nothing
   is signed or published. Install each artifact by hand (`releasing.md` smoke-check).
2. Set the secrets; rerun with `sign=true`, `publish=false`. Check the Windows job's probe and
   signature steps, the macOS job's `spctl` line.
3. Tag `v<ver>-test1`: a prerelease with all five assets appears; an installed copy does **not**
   take it.
4. Tag `v<ver>`. Confirm each updater takes it (Windows scheduled task, `nullgatectl --update
   --check`, Obtainium).

## Known soft spots
- The Windows job is the first time the ARM64 cross-build runs on a hosted image; the image has
  the ARM64 MSVC tools and LLVM, but `fetch-gtk-msys2.ps1` pulls a rolling MSYS2, so the ARM64 GTK
  can move underneath a release. `verify-bundle.ps1` is the audit.
- gvsbuild's `.pc` files name the build machine's prefix; pkgconf relocates it on Windows and the
  job proves that with `pkgconf --modversion gtk4` before building.
