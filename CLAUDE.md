# CLAUDE.md — agent guide for Nullgate (iroh-private-network)

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
| `ipn-gui` | **Nullgate** — the GTK4 + libadwaita app (binary `nullgate`), unprivileged IPC client. The product name in UI/docs is "Nullgate"; `ipn-gui` stays as the codebase codename. The **same binary** also runs as the headless **tray agent** (`nullgate --agent`, `agent.rs`): it owns the tray + notifications and launches the GUI on demand. Also owns **optional per-device actions** (`actions.rs`) — the one feature with no engine/IPC layer at all (see the gotcha). | surface the feature in the UI (window) or the tray/notifications (agent) |
| `ipn-mobile` | UniFFI facade (`cdylib` `ipn_mobile`) wrapping `ipn-core` in-process for **Android** — no daemon/IPC. Exposes `MobileEngine` + an `EventListener` callback; the engine drives the `VpnService` TUN via fd injection. | expose an engine action/event to the Android app |

The **Android app** lives in `android/` (Kotlin/Compose over `ipn-mobile`; binary product name
"Nullgate", appId `io.github.steeb_k.nullgate`). It runs the engine in-process inside a foreground
`VpnService` and routes a split tunnel for `10.99.0.0/24`. See `docs/android-packaging.md`. On
Android, `ipn-core` compiles with `keyring`/geo gated out (file-backed secrets, no geolocation) and
takes its display name from `set_device_name_override` (OS hostname is meaningless there); routing
uses `RealTun::from_fd` over the `VpnService` fd plus `Engine::{assigned_ip, attach_tun_fd,
detach_tun}` and the `EngineEvent::Tun{Setup,Teardown}Required` events.

Key module map in `ipn-core/src`: `engine.rs` (orchestration + public API), `roster.rs`
(signed membership `ipn-roster-v2` — roles Peer/Controller, invites, static-IP assignment, role
rules), `membership.rs` (roster over iroh-docs; `load_entries` also feeds the derived audit log),
`admission.rs` (PSK + SAS), `network.rs` (secret derivation + ticket w/ invite nonce), `node.rs`
(iroh node), `router.rs` + `tun_device.rs` (data plane), `conntrack.rs` (one-way "disable remote
access" block), `presence.rs` (gossip presence + access/hidden flags), `relays.rs` (custom relay
servers: per-device `relays.cbor` w/ optional Bearer tokens, preferred/only policy, the
`PreferMyRelaySelector` path selector — needs iroh's `unstable-custom-transports` feature, pinned
in the root `Cargo.toml`; `desired_relay_configs()` is the single source of the relay map for both
bind and live edits; the map is pushed to the live endpoint off the request path by
`engine::apply_relay_map`; `probe_relay()` validates a relay + token from a *throwaway* endpoint
before saving — never the live one, and it is the only way to test a token, which is checked after
the websocket upgrade, not on it).

Roles: **Originator** = master-key possession (orthogonal to roster role); roster roles are
**Peer** and **Controller**. Controllers add/remove Peers + issue Peer invites; only the
originator issues Controller invites (always single-use), rotates, deletes, or views the key. The
admin **activity log** is a 30-day view derived from the signed roster history (all tiers can
view). Per-device **disable-remote-access** (one-way, conntrack-enforced) and **hide** (UI-only
courtesy) toggles ride in presence.

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

# Android (needs JDK 17 + Android SDK 35 + NDK r27c + cargo-ndk + the 3 android rust targets):
cargo ndk -t arm64-v8a build -p ipn-core      # quick cross-compile sanity check
pwsh -File scripts/run-android.ps1            # build APK + install + launch (emulator AVD seed_api35)
#   or: cd android && ./gradlew :app:assembleDebug   ; see docs/android-packaging.md
```
Packaging + releases: see `docs/releasing.md` (+ `windows-/linux-/macos-/android-packaging.md`). From
0.1.0 we ship real installers with auto-update: a **code-signed Windows MSI** (`scripts/
build-msi.ps1`, Azure Trusted Signing), a **Linux** system-service tarball (`scripts/
package-linux.sh` + `packaging/linux/nullgatectl`), and a **macOS** universal `.app` tarball
(`scripts/setup-conda-macos.sh` once to build the conda-forge GTK env, then
`scripts/package-macos.sh`, built on a Mac). Releases are `gh release` uploads to the **public**
`steeb-k/nullgate` repo; the in-product updaters + `install.sh` read its
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
- **`insert_relay`/`remove_relay` can block for *minutes*. Never `.await` them on a request path.**
  They look like setters but each awaits iroh's bounded socket-actor channel (`mpsc::channel(256)`),
  and that actor blocks on a per-remote `RemoteStateActor` (inbox of 16), which blocks on
  `poll_send` into the relay transport, which returns `Pending` **forever** while a relay client
  can't drain — e.g. a token-gated relay answering `401`. One peer stuck on a dead relay path backs
  up the entire chain. This is what made `relay add` hang for 20+ minutes on a live mesh (and only
  there: an *idle* endpoint has no peer traffic to wedge the actor, so it reproduces in 4 ms). Two
  consequences: (1) the endpoint work belongs in a spawned task (`engine::apply_relay_map`) with a
  **per-call** timeout — never one timeout for the batch, because the calls are sequential and the
  map mutation happens on the *first poll* before the blocking await, so one stuck call would stop
  every later insert from ever being polled and leave the map half-written; (2) the whole pass is
  idempotent, which is what lets it simply be retried.
- **A relay map change does not evict the home relay you're already on.** iroh advertises exactly
  one relay (the home relay, picked by latency), and when a net-report finds nothing reachable it
  *re-injects the current home relay* as the preferred one (`handle_net_report_report` in
  `iroh-1.0.0/src/socket.rs`), so the home relay only ever **moves** — it never clears. Removing it
  from the map stops it being probed or dialed afresh, but an endpoint already homed on it stays put
  until some other relay wins a report. So switching to `RelayPolicy::Only` while your custom relay
  is unreachable leaves the daemon on the public relay it was already using — the opposite of what
  `Only` promises. There's no iroh API to force it off; `engine::settle_home_relay` therefore waits
  ~60 s and reports `RelayApply::Failed` ("restart the daemon") rather than claiming a success we
  didn't get. Don't "simplify" that away.
- **Relay settings are per-device, and a half-deployed token-gated relay partitions the network.**
  They are not distributed through the roster. A device homed on your relay is reachable *only*
  there, so a peer without the token has no relay path to it and (hole-punching being
  relay-coordinated) usually no direct one either — while the relay is perfectly healthy. This
  actually happened, for three days. `Preferred` keeps the public relays in the map to bound it;
  both UIs warn; don't remove the warning.
- **The tray + notifications live in the agent, NOT the daemon or the GUI.** A system service can't
  draw UI in the user session (Windows session 0, root systemd/LaunchDaemon), so the tray can't be
  in the daemon; and tying it to the GUI made it vanish on close/crash. The persistent user-session
  piece is `nullgate --agent` (`ipn-gui/src/agent.rs`) — it owns `tray.rs` + `notify.rs` and
  launches the GUI on demand. When adding a tray item or notification, put it in the **agent**; the
  GUI window keeps only in-app `adw::Toast`s. The agent is a headless GApplication with a distinct
  app id (`…Nullgate.Agent`) and must stay `hold()`-ed (keep the guard, or `mem::forget` it — a
  dropped `ApplicationHoldGuard` quits it). The agent must be *running* for the tray to exist, so
  it's (re)launched from every session angle — **login** autostart (`--agent`), **GUI start**
  (`spawn_agent` in `main()`), and the **installers on install/upgrade** (`nullgatectl`
  `launch_agent_for_user`/`gui_agent_reload`; Windows via the updater's user-session relaunch).
  Single-instance makes every one of these a safe no-op if the agent is already up. The **daemon
  never launches it** (session 0 can't draw UI, and the agent's lifetime is independent of the
  daemon).
- **GApplication single-instance does nothing on macOS.** GLib implements it over a D-Bus session
  bus, which macOS has none of, so *every* launch becomes its own primary. Both long-lived roles
  rebuild it on `flock` in `macos_single_instance` (`ipn-gui/src/main.rs`): the **agent** takes
  `/tmp/nullgate-agent-<uid>.lock` (without it, each GUI start left another agent and another tray
  icon), and the **GUI** takes `/tmp/nullgate-gui-<uid>.lock` plus a `…-gui-<uid>.sock` that a second
  launch pokes so the existing window presents itself (without it, the tray's *Open Nullgate* opened
  a duplicate window). Do **not** "simplify" `launch_gui` to `open -a Nullgate.app`: the agent runs
  the bundle's `CFBundleExecutable`, so Launch Services thinks the app is already running and
  activates the *headless agent* — the window never appears. Two corollaries for `nullgatectl`:
  stray GUI-spawned agents are not launchd jobs, so `launchctl bootout` won't stop them
  (`gui_agent_kill` pkills them and waits for the lock's fd to close); and `launchctl bootstrap` is a
  **no-op on an already-loaded job**, so always `bootout` first or a changed plist never takes effect
  (this stranded machines on the pre-0.2.0 `--minimized` job for months). The GUI is a normal single-instance window (closing it quits only the GUI). Verify on Windows:
  close the GUI → tray icon stays; the tray's *Open Nullgate* / *Restart Nullgate daemon* / *Quit
  Nullgate* all work; a notification click opens the window.
- **The GUI updates widgets in place — never reintroduce full-page rebuilds.** The main page is a
  build-once tree (`MainPage`/`build_main_page` in `ipn-gui/src/main.rs`); `apply_status` diffs
  member rows by node id, restyles dots/subtitles on live widgets, and re-sorts a `gtk::ListBox`
  (which moves rows without destroying them). History: the page used to be torn down + rebuilt on
  any change while status pushed several times a second — clicks landed on widgets destroyed
  mid-press ("clicking doesn't work") and keyboard focus snapped back to "Administration" (regressed
  ~4 times). Rules when touching the GUI: (1) update in place; if a panel must rebuild (only the
  admin flyout does), gate it on a signature of ONLY what it displays (`admin_signature`) and never
  put per-tick-volatile fields (last-seen, `observed_addr`, `direct`, IPs) in that signature;
  (2) keep the focus save/restore around `apply_status` (`focused_row_title`/`focus_row_by_title`);
  (3) **check on every build**: run the Windows GUI with a peer connected, rapid-click member rows
  (every click must land) and tab the member list (selection must NOT snap back to
  "Administration"). Engine/daemon side of the same story: presence mutators report whether
  user-visible state changed and `Changed` is only emitted then (plus a ~30s catch-all tick), and
  the daemon coalesces event bursts into one status push per 250ms quiet window — don't add
  unconditional `Changed` emits to hot paths.
- **A sleeping macOS laptop is not an offline one.** macOS takes a **dark wake** every few minutes on
  battery (Power Nap maintenance — *not* the "wake for network access"/`womp` setting, which is off
  on battery by default). The daemon is frozen, not stopped, so each dark wake used to resurrect the
  mesh for seconds and make every other device announce "came online" all night. `ipn-daemon/src/
  power.rs` now leaves the network before sleep and rejoins only on a **full** wake. Two things to
  know: (1) it must use `IOPMConnection` (`power/macos.rs`), **not** the documented
  `IORegisterForSystemPower` — the latter reports dark wake and real wake identically
  (`kIOMessageSystemHasPoweredOn` for both), which is precisely the distinction the fix needs; the
  IOPMConnection symbols are exported from IOKit but headerless, so the FFI is transcribed from
  Apple's open-source `IOPMLibPrivate.h` (only the low capability bits are used — they're
  cross-checked against the public `IOPM.h`). (2) The sleep callback **must finish the disconnect
  before it acknowledges** the event, or powerd freezes the machine mid-teardown. The `notify.rs`
  online-debounce cannot substitute for any of this: a laptop asleep for hours clears any debounce.
- **Per-device action buttons live in the GUI, NOT the daemon — on purpose.** `ipn-gui/src/
  actions.rs` stores them in the *user's* config dir (`actions.json`), and they never touch
  `ipn-core`/`ipn-ipc`. This looks like a violation of the engine-first workflow, and the obvious
  "fix" — a `SetAction` IPC request + an `actions.cbor` in the daemon, mirroring the existing
  `nicknames.cbor`/`notes.cbor` — is a **security regression**, so don't. A nickname is inert data;
  an action is an *executable command line*, spawned later by whichever user's GUI reads it back. The
  daemon runs as SYSTEM/root and its IPC socket is reachable by any local user, so putting exec
  strings behind it turns an inert local surface into a cross-user code-execution path. Keep them
  per-user, and keep the spawn shell-free (direct `Command`, quotes group, backslashes literal,
  placeholders expanded **per-token after** the split so a peer-supplied `hostname` can't become a
  second argument). Consequence: no CLI command for this feature (the daemon has no such state), and
  the tray agent — a different process from the GUI that edits the file — re-reads it on a 1s mtime
  poll.
- **Two traps inside `actions.rs` that look like tidying-up.** (1) The Windows "open in a terminal
  window" path sets `CREATE_NEW_CONSOLE` and **deliberately leaves stdio alone**. Adding the
  `Stdio::null()` calls the detached path uses (for symmetry — they look missing) makes Rust set
  `STARTF_USESTDHANDLES`, and the child then gets its console window *and* streams pointing at `NUL`:
  a terminal you can type into that never answers. A GUI-subsystem process has no std handles, which
  is exactly why omitting them lets Windows wire the child to the new console. (2) The 8 colors are
  the app's only *dynamic* CSS provider (regenerated on `StyleManager::notify::dark`), because GTK4
  CSS has no `@media` and libadwaita only auto-swaps its own named colors. The palette is **one vivid
  hex per color** (`ActionColor::vivid`), and everything else is *derived* from it: the 1px border is
  the vivid itself, the interior is it tinted toward white (light) or black (dark), and the text
  color follows the **theme, not the hue**. Don't reintroduce a per-hue text color or hand-pick the
  fills — a per-hue text color is what made yellow the one button that didn't match. Tests hold every
  fill (rest/hover/pressed × both themes) to WCAG AA and every border to a visible contrast against
  the card; they are what caught that a *pure* vivid yellow border is ~1.5:1 on a white card, which
  is why Yellow is a gold. Note `.ng-action` in `style.css` deliberately sets **no `border`** — the
  generated color classes own it, and a `border: none` there would flatten every button.
- **GTK on Windows** comes from gvsbuild at `C:\gtk`; `pkg-config` must resolve `gtk4` and
  `libadwaita-1`. On Linux, install the `-dev` packages.
- **GTK on macOS** comes from **conda-forge**, not Homebrew (`scripts/setup-conda-macos.sh` builds
  `.conda-gtk/{arm64,x86}`) — conda-forge's dylibs carry `minos 11.0` so the shipped `.app` runs on
  macOS 11+, whereas Homebrew stamps the build host's OS (e.g. `minos 26` on a macOS 26 box). Needs a
  `micromamba`/`mamba`/`conda` on PATH. The bundled `.app` finds its resources via `ipn-gui`'s
  `setup_runtime_env()` (sets `GSETTINGS_SCHEMA_DIR`/`GDK_PIXBUF_MODULE_FILE`/`FONTCONFIG_PATH`).
- **Windows service install needs UAC** and can't be exercised headlessly — verify the IPC path
  with the foreground daemon + `ipn-cli` instead.
- `.gitattributes` forces LF on `*.sh` so the WSL/Linux scripts survive a Windows checkout.
- Don't run the GUI as root/sudo on Linux (it loses the display); privilege belongs to the
  daemon (via `setcap`/service), not the GUI.
- Commit/push only when asked. Releases are `gh release` uploads of locally-built artifacts.
