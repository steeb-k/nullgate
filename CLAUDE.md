# CLAUDE.md — agent guide for iroh-private-network (Nullgate)

Read this before changing anything. It is the authoritative guide for how this repo is built,
how features are added, and how they must be documented. Keep it accurate — if you change the
architecture or workflow, update this file in the same change.

## What this is
A peer-to-peer virtual LAN over [iroh](https://www.iroh.computer): connect your own devices into
a private `10.99.0.0/24` network so you can RDP/SSH/etc. to one machine directly, instead of
routing all traffic through a home VPN. No accounts, no central server. Desktop today (Windows +
Linux; macOS buildable, Android planned).

User-facing intro: `README.md`. Design: `docs/architecture.md`, `docs/security.md`. Build/
packaging: `docs/building.md`. How to contribute a feature: `docs/development.md`.

## Architecture (where things live)
Rust workspace; each crate has one job. A feature usually flows through these layers in order:

| Crate | Role | You touch it to… |
|-------|------|------------------|
| `ipn-core` | Engine: iroh node, signed roster (iroh-docs), admission + emoji SAS, presence, TUN routing. UI/IPC-agnostic. | add/change actual behavior; **all security logic lives here** |
| `ipn-ipc` | Wire protocol (`IpcRequest`/`IpcResponse`/`IpcEvent`) + transport (named pipe / unix socket). Depends on `ipn-core` only for display DTOs. | expose a new engine action/event to clients |
| `ipn-daemon` | Privileged process: owns the engine + TUN, serves IPC. Windows service + foreground modes. | route a new request to the engine |
| `ipn-cli` | Headless IPC client (testing/scripting). | add a command for the new action |
| `ipn-gui` | **Nullgate** — the GTK4 + libadwaita app (binary `nullgate`), unprivileged IPC client. The product name in UI/docs is "Nullgate"; `ipn-gui` stays as the codebase codename. | surface the feature in the UI |

Key module map in `ipn-core/src`: `engine.rs` (orchestration + public API), `roster.rs`
(signed membership + role rules), `membership.rs` (roster over iroh-docs), `admission.rs`
(PSK + SAS), `network.rs` (secret derivation + ticket), `node.rs` (iroh node), `router.rs` +
`tun_device.rs` (data plane), `presence.rs` (gossip presence).

## Build / run / test
```sh
# Dev (two processes): daemon is privileged, GUI is not.
#   Linux once: sudo setcap cap_net_admin,cap_net_raw+ep target/debug/ipn-daemon
cargo run -p ipn-daemon            # Windows: from an elevated shell
cargo run -p ipn-gui

cargo build --workspace            # everything
cargo test -p ipn-core             # unit tests (fast, no network)
# e2e tests open real iroh endpoints, so they are #[ignore]d:
cargo test -p ipn-core --test engine_e2e -- --ignored
cargo test -p ipn-core --test delete_e2e -- --ignored
cargo test -p ipn-core --test rotate_e2e -- --ignored
```
Packaging + releases: see `docs/releasing.md` (+ `windows-/linux-/macos-packaging.md`). From
0.1.0 we ship real installers with auto-update: a **code-signed Windows MSI** (`scripts/
build-msi.ps1`, Azure Trusted Signing), a **Linux** system-service tarball (`scripts/
package-linux.sh` + `packaging/linux/nullgatectl`), and a **macOS** universal `.app` tarball
(`scripts/package-macos.sh`, built on a Mac). Releases are `gh release` uploads to the **public**
`steeb-k/iroh-private-network` repo; the in-product updaters + `install.sh` read its
`releases/latest`. The signing metadata (`artifact-signing-metadata.json`) is **git-ignored** —
never commit it. Builds are **local** (Windows native; Linux/Android via WSL; macOS on a Mac).
**Do not add GitHub Actions / CI.**

## Adding a feature — the workflow
1. **Engine first.** Implement the behavior as a method on `Engine` in `ipn-core` (or a new
   module). Emit `EngineEvent::Changed` (or a specific event) when state changes. Keep all
   mutable state behind the existing async `Mutex`; do network I/O off-lock.
2. **Tests with it.** Add `#[cfg(test)]` unit tests in `ipn-core`. If the feature affects
   membership, connectivity, or revocation, add or extend an **ignored e2e smoke test** under
   `crates/ipn-core/tests/` that proves the real-world property (see `delete_e2e`/`rotate_e2e`
   as templates — especially: a removed/booted device must end with **zero live connections**
   and no visibility).
3. **Expose over IPC.** Add a variant to `IpcRequest` (and `IpcResponse`/`IpcEvent` if needed)
   in `ipn-ipc`, then handle it in `ipn-daemon`'s `handle_request` / `map_event`.
4. **CLI.** Add the command to `ipn-cli` (cheapest way to test the IPC path headlessly).
5. **GUI.** Add a `UiMsg`/control path in `ipn-gui` and render it. Never block the GTK thread —
   issue requests via `Net::request` and update on the `async-channel`.
6. **Document it (required, same change).** See the rule below.
7. **Build the installers** for each platform on its own OS (`scripts/build-msi.ps1` on Windows,
   `scripts/package-linux.sh` via WSL, `scripts/package-macos.sh` on a Mac) before a release.

### Definition of done
A feature isn't done until: it compiles on Windows **and** Linux; `cargo test -p ipn-core`
passes and any relevant e2e smoke test passes; **docs are updated**; a `CHANGELOG.md` entry is
added.

## Documentation rules (do this in the same change as the code)
- **User-visible behavior** → update `README.md` ("What it does" / "Using it") in plain,
  non-jargon language aimed at a mildly-technical user. Keep implementation detail out of it.
- **Design/mechanism** → update the right file in `docs/` (`architecture.md` for components/
  data flow, `security.md` for trust/identity/revocation, `building.md` for build/test,
  `releasing.md` for packaging/release).
- **Always** → add a `CHANGELOG.md` entry under `## [Unreleased]`.
- Keep README friendly and `docs/` precise. If you move detail out of README, link to the doc.
- If you change crates, commands, or this workflow, update this `CLAUDE.md`.

## Conventions
- Errors: `anyhow` in engine/daemon/cli; `io::Result` in transport; the GUI maps failures to a
  toast. Don't `unwrap()` on fallible I/O in long-running paths.
- Comments explain **why**, not what; match the density/voice of the surrounding file.
- The iroh ecosystem crates (`iroh`, `iroh-docs`, `iroh-gossip`, `iroh-blobs`, `iroh-tickets`,
  `iroh-mdns-address-lookup`) are pinned **together** in the root `Cargo.toml` and must be
  bumped together — after a bump run `cargo tree -d` and confirm a single `iroh-base`.
- A member's signing key **is** its NodeId (ed25519); the originator master key is separate.
- Verified iroh 1.0 API notes live in the maintainer's agent memory; when unsure, read the
  cached crate source under `~/.cargo/registry/src/.../iroh-1.0.0` rather than guessing.

## Gotchas
- **TUN needs privilege.** Tests and headless runs set `NULLGATE_DISABLE_TUN=1`; the engine honors
  it and skips creating a real interface. Always set it in automated tests.
- **GTK on Windows** comes from gvsbuild at `C:\gtk`; `pkg-config` must resolve `gtk4` and
  `libadwaita-1`. On Linux, install the `-dev` packages.
- **Windows service install needs UAC** and can't be exercised headlessly — verify the IPC path
  with the foreground daemon + `ipn-cli` instead.
- `.gitattributes` forces LF on `*.sh` so the WSL/Linux scripts survive a Windows checkout.
- Don't run the GUI as root/sudo on Linux (it loses the display); privilege belongs to the
  daemon (via `setcap`/service), not the GUI.
- Commit/push only when asked. Releases are `gh release` uploads of locally-built artifacts.
