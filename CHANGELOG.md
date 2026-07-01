# Changelog

All notable changes to Nullgate. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]
### Added
- **macOS build (first published artifact).** Nullgate now builds a self-contained, universal
  (arm64 + x86_64) `Nullgate.app` tarball for macOS. GTK is bundled from **conda-forge** rather
  than Homebrew so the dylibs carry `minos 11.0` and the app runs on macOS 11+, regardless of the
  build host's OS (a Homebrew build on a modern Mac would stamp an unusably high floor). New
  `scripts/setup-conda-macos.sh` creates the conda-forge GTK env(s); `scripts/package-macos.sh`
  and `scripts/bundle-gtk-macos.sh` were reworked to source GTK from those envs
  (`MACOSX_DEPLOYMENT_TARGET=11.0`, `-headerpad_max_install_names`, `@loader_path` handling,
  `BUNDLE_SKIP_AUX` for the universal lipo pass). The GUI gained a macOS `setup_runtime_env()`
  that points bundled GTK at the `.app`'s relative schema/pixbuf-loader/fontconfig dirs. The
  one-liner `install.sh` already handles macOS. See `docs/macos-packaging.md`.

## [0.2.0]
### Added
- **Android app (initial).** Nullgate now builds and runs on Android: a Kotlin/Compose UI over a
  new `ipn-mobile` UniFFI facade that runs `ipn-core` in-process inside a foreground service — no
  separate daemon. Full feature parity with the desktop app (create/join, roster with live
  presence, emoji-SAS join approval, tickets incl. single-use + controller, member roles, freeze/
  rotate/delete, originator-key export/import, per-member nicknames/notes, hide / disable-remote-
  access, activity log). Packet routing is real: the app drives Android's `VpnService` and feeds
  its TUN file descriptor into the engine (`Engine::attach_tun_fd`) over a split tunnel
  (`10.99.0.0/24` only, so normal phone traffic is unaffected). Join tickets can be pasted or
  scanned/shown as QR. Build/run via `scripts/run-android.ps1`; see `docs/android-packaging.md`.
- **`ipn-core` Android support.** `tun_device::RealTun::from_fd` adopts a `VpnService` fd;
  `EngineEvent::TunSetupRequired` / `TunTeardownRequired` coordinate VPN bring-up/teardown;
  `Engine::{assigned_ip, attach_tun_fd, detach_tun}` expose the fd-injection path;
  `set_device_name_override` supplies a stable display name (the Android OS hostname is
  meaningless). On Android secrets are file-backed (no OS keystore) and the geolocation stack is
  not compiled in.

### Fixed
- **Live roster sync: new members, removals, and the activity log now appear without a restart.**
  The roster-doc's iroh-docs live-sync gossip swarm was only ever seeded pairwise at join time
  (joiner↔bootstrap, approver→joiner) and never refreshed, so a later Add/Remove/role change — and
  the activity log derived from them, and a device learning it was *itself* removed — only reached
  members with a healthy direct link and was otherwise missed until the app was fully quit and
  restarted. The maintenance tick now re-seeds the swarm with **all** current members (on change
  and every ~8s), so changes propagate within seconds. Affects desktop and Android.
- **Android: enabling routing failed with "there is no reactor running…".** The facade's
  `attach_tun`/`detach_tun` ran outside the tokio runtime, but adopting the `VpnService` fd
  registers it with the reactor (`AsyncFd`) and spawns the pump; they now run inside `block_on`.
- **Android: the joiner's emoji verification screen now appears before approval**, not after. The
  SAS is emitted during the handshake (before the network activates), but the UI only rendered it
  inside the member list, which doesn't exist until the join is accepted; it's now a top-level
  overlay shown while joining.
- **Windows: pin the GLib program/application name to "Nullgate"** so the running GTK process can't
  surface under the crate codename. (The executable already embeds `FileDescription`/`ProductName`
  = "Nullgate"; if Task Manager still shows the old name, Windows is caching stale version info for
  that exe path.)

## [0.1.8]
### Fixed
- **Per-member notes: styling and live updates.** The Notes editor is now a rounded card inset
  from the panel edges (it had sharp corners and ran off the bottom). Typing a note and reopening
  the flyout — or returning to the member page — now shows the text and refreshes the row's preview
  **immediately**, instead of only after a full status refresh (the member flyout isn't rebuilt
  while it's open, so the GUI now caches the edit locally and updates the open row directly).

## [0.1.7]
### Added
- **Per-member notes.** Each member's detail page now has a **Notes** entry (below Status, for
  members other than this device) that opens a full-height editable text area. Notes are stored
  **locally** and never shared with other members (like nicknames). They autosave when you leave
  the field. CLI `nullgate-cli note <node-id> [text]`.

### Changed
- **Flyout Back steps back one level** instead of jumping to the main page. Drilling member →
  Notes and hitting Back now returns to the member page (Back again closes the flyout). Alt+Left,
  Backspace, and dismissing the flyout all follow the same history.

### Fixed
- **Linux launcher/dock icon now resolves.** The `.desktop` was missing `StartupWMClass`, so the
  desktop environment couldn't tie the running window to the launcher entry and fell back to a
  generic/broken icon. Added `StartupWMClass=io.github.steeb_k.Nullgate` (matching the app ID, as
  GTK reports it). Also regenerated the multi-size Windows `.ico` from the per-size PNGs.
- **Using a spent or expired join code now gives a clear message instead of silently failing.**
  Single-use codes (and stale codes superseded by regeneration) are validated at the moment of
  joining: the joiner is told *"This invite code has already been used / is no longer valid — ask
  for a new one,"* and the existing member isn't even prompted to approve a dead code. As a
  backstop, a joiner that's approved but then rejected by the roster fold (e.g. a code consumed by
  a simultaneous join) no longer lands in a half-joined limbo — it reports the failure and cleanly
  backs out instead of sitting "in" the network with no IP. IPC protocol bumped to **v3** for the
  new note request.

## [0.1.6]
### Added
- **Privilege tiers — Originator, Controller, Peer.** Membership now carries a role. **Peers**
  use the network and view the activity log, but can't approve devices or view join tickets.
  **Controllers** behave like the old members: they add/remove Peers and issue Peer-level
  tickets, but can't view the originator key, rotate the secret, or delete the network.
  **Originators** (master-key holders) keep full authority and additionally issue Controller
  tickets and promote/demote members. Any tier can still import the originator key to *become* an
  originator. New roster ops `SetRole`/`SetInvite`; CLI `nullgate-cli role <node-id> <peer|controller>`.
- **Tiered, invalidatable join tickets.** "Peer management" (formerly "Show join ticket") shows
  "Show join ticket (Peer level)" to Controllers and additionally "Show join ticket (Controller
  level)" to Originators, each with a hover tooltip explaining the tier. **Controller tickets are
  always single-use** (consumed once in the shared roster). **Peer tickets** have an Administration
  toggle for single-use (default off); flipping it — or issuing a Controller ticket — mints a fresh
  code that **invalidates the previous one for new joins, without disconnecting anyone**.
- **Built-in administration activity log.** A new "Activity log" view (visible to every member)
  lists the last 30 days of administrative actions — adds, removals, role changes, invite
  regenerations, freezes, renames — each with the time, the actor, and what they did. It's derived
  from the signed roster history, so it's tamper-evident and identical for everyone. CLI
  `nullgate-cli log`.
- **Per-device access switches (Controllers/Originators).** On your own device page:
  **"Disable remote access"** — a one-way block (stateful connection tracking) so you can still
  reach other members but no one can initiate to you; your row turns its dot yellow, shows "Access
  disabled", and drops below other online devices. **"Hide this device from member list"** —
  implies the block and removes you from the list for Peers and Controllers (Originators still see
  you, with a white dot and "Hidden", and still can't reach you). Hiding is a presentation courtesy
  (the roster is a shared signed log), not a security boundary — the access block is the real
  enforcement.
- **Static virtual IPs.** A device keeps its `10.99.0.x` address for the life of its membership;
  it only changes if the device leaves and rejoins. The admitter records the address in the signed
  `Add` and the fold honors it, so another device joining or leaving never shifts yours.

### Changed
- **Roster format bumped to v2 (`ipn-roster-v2`).** Roles, invites, and static IPs are baked into
  the signed entries, so this is a clean break: **existing networks must be recreated** (re-create
  on the originator and re-invite devices). Presence heartbeats also carry the new access/hidden
  flags — upgrade all devices together.

### Fixed
- **The one-way block now cuts already-open inbound sessions, not just new ones.** "Disable remote
  access" tracks the connections *you* initiate and only lets matching return traffic back in — but
  it was recording your own services' replies as if you'd initiated them, so a session someone
  already had *into* your machine kept working after you flipped the block on. It now only treats a
  flow as self-initiated on a real client open (a TCP SYN, or the first packet of a UDP flow);
  server-side replies just refresh an existing flow. So enabling the block severs in-progress
  inbound sessions while your own outbound sessions stay up. The daemon also logs when the block is
  toggled and how many inbound packets it drops.
- **The "Hide this device" switch now turns on and locks the "Disable remote access" switch**
  (its enabling is implicit), and the IPC protocol version was bumped to **2** so a GUI can't
  silently talk to an older daemon that doesn't understand the new tier/access requests.
- **Windows desktop notifications work again.** They were silently dropped (a workaround for a
  stray second tray icon). Nullgate now shows **native Action Center toasts** on Windows — the
  "running in the tray" notice the first time you close the window, plus peer-online and
  join-request alerts — via WinRT instead of GLib's `GNotification` (whose Windows backend spawned
  the extra tray icon). The app registers an AppUserModelID (`io.github.steeb_k.Nullgate`) at
  startup and on the Start-menu shortcut so toasts are attributed to Nullgate. The 30s
  same-message throttle now applies on all platforms.
- **Verification-code emoji render in color on Windows** (e.g. ✂️ showed as a tofu box). The SAS
  emojis now pin an emoji-capable font (Segoe UI Emoji / Noto Color Emoji / Apple Color Emoji)
  rather than inheriting the pinned `Segoe UI Variable Text` UI font, which lacks several glyphs.
- **Rebrand leftovers that the OS/UI still showed as "ipn".** Task Manager listed the GUI process
  as `ipn-gui` because the embedded Windows version-info strings (FileDescription/ProductName)
  still defaulted from the crate name — they're now pinned to **Nullgate**. The TUN network
  adapter is now named `nullgate` (was `ipn`) in Network Connections / `ip link`, and the
  fallback device name shown to peers when the OS hostname is unreadable is now `nullgate-device`.

## [0.1.4]
### Changed
- **Join-ticket and recovery-code prefixes rebranded.** Tickets now start `ng1…` and originator
  recovery codes `ngkey1…` (were `ipn1…`/`ipnkey1…`), finishing the Nullgate rename. Tickets/codes
  minted by 0.1.3 or earlier won't parse — re-share the ticket from a 0.1.4 device.

## [0.1.3]
### Changed
- **Rebranded to "Nullgate".** The application is now **Nullgate** everywhere it's visible — the
  window/About/tray/notifications, the Windows service (**NullgateDaemon**) and scheduled task
  (**NullgateUpdate**), the **process names** (`nullgate`, `nullgate-daemon`, `nullgate-cli`), the
  Linux/macOS manager (**`nullgatectl`**), the app-id (`io.github.steeb_k.Nullgate`), install
  paths (`Program Files\Nullgate`), the data dir / IPC socket, and the `NULLGATE_*` env vars.
  **Fresh start:** the new identity/paths mean existing local networks are not carried over —
  remove the old install first (Windows: uninstall the old MSI; Linux: old `--uninstall`) and
  re-create your network. The repo stays `iroh-private-network`; crate names stay `ipn-*`.
- **New app icon ("stacked").** The window/taskbar/launcher icon now uses hand-tuned per-size
  art (16–512px) across all platforms: a multi-size Windows `.ico`, per-size Linux hicolor PNGs,
  and a per-size macOS `.icns`. (The tray icon is unchanged.)
- **New tray icon** (`tray-icon-splash.png`) — a full-color icon, used as-is on every theme.

### Fixed
- **Duplicate desktop notifications are throttled** to once per 30s per message — fixes the burst
  of repeated "came online" toasts when a peer flaps offline/online during an update.
- **Linux launcher icon now shows.** `nullgatectl` installs the `.desktop` + hicolor icons under
  `/usr/share` (which is always in `XDG_DATA_DIRS` and has the theme's `index.theme`) instead of
  `/usr/local/share`, so the icon resolves and `gtk-update-icon-cache` works.

## [0.1.2]
### Changed
- **Branding: the desktop app is "Nullgate".** The main window header now reads **Iroh Private
  Network** with **Nullgate <version>** beneath it. "Nullgate" is the product name for the GUI
  in the UI and docs; `ipn-gui` stays as the codebase codename.

### Added
- **The GUI restarts itself after an auto-update** so you're not left on the old version. The
  daemon now reports its app version over IPC; when it comes back newer, the GUI relaunches —
  Linux/macOS re-exec the (in-place-swapped) binary, Windows uses the installer's Restart Manager
  (`RegisterApplicationRestart`) to close/replace/restart it. Tray-minimized state is preserved
  (it reopens minimized if it was minimized, otherwise on screen).

### Changed
- **Default tray icon is now the color icon** (`icon-tray-color.png`). A monochrome variant
  (`icon-tray-mono.png`) is bundled for a future Settings toggle.

## [0.1.1]
### Added
- **Launch on login, minimized to the tray.** Installs an auto-start entry so the GUI comes up
  hidden in the tray at each login — Windows (per-machine Run key in the MSI), Linux (XDG
  autostart, already added by `nullgatectl`), macOS (login LaunchAgent). The daemon already auto-starts,
  so a reboot brings the tray up with the network live; click the tray icon to open the window.

### Fixed
- **Linux/macOS updater aborted with exit 23.** `nullgatectl --update` used `set -o pipefail` with
  `curl | grep -m1 …` extraction pipelines; the early-closing consumer made curl exit with EPIPE
  (code 23), which `pipefail` propagated and `set -e` turned into an abort — even though the value
  was read fine. Dropped `pipefail` (results are guarded; critical ops use `|| die`).
- **No console window on Windows** alongside the GUI (release builds are a GUI-subsystem binary).
- **Friendly name updates immediately** in the open member-detail flyout when set (no need to
  close and reopen it).
- **One tray icon on Windows.** Removed a stray, do-nothing second tray icon (GLib's
  `GNotification` backend was creating its own); the tray icon **opens the window on double-click**
  (and a left single-click), with the Open/Quit menu on right-click. Desktop notifications are
  skipped on Windows for now — the tray icon and the in-app "Join Request" chip still signal
  events (native WinRT toasts are a follow-up).

## [0.1.0]
### Changed
- **About dialog cleaned up.** Shows the app icon and the name "Nullgate", developer
  "kznjk", and **Website** + **Report an Issue** links (to the GitHub repo / issues); the
  "Details" page is gone. The About row no longer shows a "›" chevron (it opens a dialog, not a
  flyout). The bundled `icon-spin` is registered into the icon theme at startup so it appears in
  the About dialog and as the window icon on all platforms.
- **Friendly names are now local nicknames.** A nickname is set by *each client for other
  members* and stored **locally** (never broadcast); the **hostname** is the shared identifier.
  (Replaces the old self-set, broadcast label.) Set it from a member's detail page; CLI
  `ipn-cli nickname <node-id> [name]`.
- **Join requests live under Administration.** No standalone section; a **flashing red "Join
  Request" chip** appears on the Administration row when one is pending, and the request shows as
  the first item in the Administration flyout on a light-red background.
- **Member detail** reordered and expanded: **Status** (with a colored dot) at top, then friendly
  name, hostname, virtual IP, **Local IP**, **Public IP** (from iroh's known addresses), and
  **Observed address** at the bottom.
- **Status dots are color-coded** everywhere: green (online) / gray (offline) / **red (offline
  > 1 week)**. Last-seen is persisted so the red state survives daemon restarts.
- **New app icon** (`img/icon-spin.*`): embedded in the Windows `.exe` and installed as the Linux
  hicolor icon. The tray icon is unchanged. (Takes effect on the next build.)
- **GUI redesigned (SEED-style).** A static "Nullgate / Nullgate" titlebar; a stylesheet
  borrowed from seed-sync-gtk (frameless header that merges into the window background, with a
  Windows-11 layer — Segoe UI, accent, rounded controls, native-style window buttons). Sub-menus
  are **overlay flyouts** (`adw::OverlaySplitView`, kept collapsed) that slide in over the **full
  window height** (over the header too) with a scrim — the window stays visible behind them —
  rather than full-page swaps. The main screen is a control group — **Administration (top) → Show
  join ticket → Diagnostics → About (bottom)** (About opens the usual dialog), with Join requests
  surfaced above when present — over a **Members** list at the bottom (this device included, shown
  first). Clicking a member opens a **detail flyout** (full info + copy + the kick button);
  clicking "this device" lets you set its friendly name. **Renaming the network lives under
  Administration** (shared across members). The **"+" create/join button is hidden once you're in
  a network**, and the hamburger menu is gone. The join ticket is no longer auto-shown on create.
- **Hostname is now the live OS hostname.** The name shown for a device is re-read from the OS on
  every presence heartbeat (and for the local device on every status), so it always reflects the
  *actual current* hostname and is never user-editable — it's the source of truth.
- **Secrets at rest now use the OS keystore.** The device key, network secret, and originator
  master key are stored via `keyring` (Credential Manager / Keychain / Secret Service), with a
  `0600`-file fallback for headless hosts and a marker that refuses to silently regenerate
  identity when the keystore is unavailable. `network.cbor` no longer contains any secret bytes.
  NOTE (pre-release): existing local networks must be re-created, since the old plaintext
  on-disk secrets are not migrated.

### Fixed
- **Public IP now shows for other members.** Each device **advertises its own public IP** in the
  signed presence heartbeat (the same value it shows for itself), so peers display it even over a
  relay path — previously it was only filled in when iroh happened to observe a direct internet
  path, so it was usually blank.
- **Joiner doesn't show the network until accepted.** Activation is deferred until the join is
  approved, so a pending joiner stays on the empty screen and a decline leaves it there (no stale
  network view, no close/reopen needed).
- **Emoji code is laid out consistently.** The SAS now renders in a fixed, symmetric **2 / 3 / 2**
  grid on both the joiner's "Verify this code" dialog and the originator's join-requests flyout
  (they previously wrapped differently — 3/3/1 vs 3/4 — depending on container width).
- **Declined join now resets the joiner to no-network.** The joiner was provisionally activated
  before the decision and a decline (or handshake failure) left it lingering "in" the network
  (showing the originator as a member, unable to retry). It now tears the activation back down on
  any post-activation failure. New `join_denied_e2e` smoke test.
- **GUI notifications are readable on Linux.** The message is now in the notification **title**
  (many Linux daemons hide/clip the body, so it previously showed only the app name). Also dropped
  the redundant "no network" toast at startup, and the **Join-requests flyout shows the emoji code
  large** (matching the joiner's "Verify this code" screen).
- **GUI focus no longer jumps / clicks no longer get stolen.** The main page was torn down and
  rebuilt on every status push from the daemon (every couple seconds), which reset keyboard focus
  (to "Administration") and could eat a click landing mid-rebuild. The UI now re-renders only when
  the displayed data actually changes (memoized by a content signature), so idle ticks are no-ops.
- **Roster-doc spam safeguard (partial).** Folding the roster now ignores non-`e/` keys, skips
  oversized entry values, and caps how many entries it processes, so a member spamming the shared
  doc can't OOM/peg others. (Bounding on-disk growth needs originator snapshot/prune compaction —
  tracked in TODO; rotate/remove is the backstop.)
- **TCP MSS clamping.** TCP SYNs (both directions) are clamped to the tunnel's MSS so flows like
  RDP/SSH/file copy never produce segments too big for a QUIC datagram (which were silently
  dropped); oversized drops are now logged. Unit tests cover the clamp + checksum recompute.
- **Protocol version negotiation.** The member mesh/join handshake exchanges a protocol version
  and rejects a mismatch with a clear, mutual error instead of a confusing connection failure;
  the GUI also version-checks the daemon over IPC and shows a "version mismatch" page. New
  `protocol_version_e2e` smoke test.
- **Roster timestamp hardening (partial).** Entries dated implausibly far in the future are
  dropped, and a member can't sign an `Add` backdated to before its own admission. (The deeper
  backdate-past-a-freeze case still needs causal ordering — tracked in TODO.)
- **Virtual-IP assignment race.** Member IPs are now derived deterministically from the NodeId
  during the roster fold (collision-free and identical on every node), instead of being chosen
  by whoever approves the join — so two simultaneous approvals can no longer assign the same
  address. New `roster` unit tests cover determinism and the concurrent case.

### Added
- **Real installers with auto-update (0.1.0).** Windows: a **code-signed MSI** (`scripts/
  build-msi.ps1`, WiX + Azure Trusted Signing) that installs to `Program Files\Nullgate`, registers
  the `NullgateDaemon` service + a daily `NullgateUpdate` scheduled task, and adds shortcuts. Linux/macOS:
  a one-line installer (`curl … | sh`) — Linux installs a root systemd service + daily update
  timer; macOS installs an `/Applications` app + root LaunchDaemon + updater. The daemon's
  privilege (TUN/utun) is handled per-OS. All three keep themselves updated from the public repo.
  New `wix/`, `packaging/`, `scripts/{build-msi,sign-artifacts,bundle-gtk-macos,package-macos}`,
  top-level `install.sh`, and `docs/{windows,linux,macos}-packaging.md`.
- **Member geolocation (Location).** Each member detail shows a **Location** ("City, State,
  Country", dropping whichever parts are unknown) under Public IP. The **originator** downloads the DB-IP City database (~60 MB, **CC BY 4.0**),
  resolves every member's advertised public IP, and **propagates the resolved strings** (signed
  with the originator master key) to everyone — members need no database and make **no external
  calls**. Refreshed bi-weekly. The required attribution ("IP Geolocation by DB-IP", linking to
  db-ip.com) sits inline next to the Location header, with an "approximate, based on the public
  IP" note as a tooltip on a help icon after the value. New `geo` module + `geo_e2e` smoke test.
- **Rename the network (shared).** The network name can be changed after creation and propagates
  to all members via the signed roster (a new `SetName` op; any current member, last-writer-wins).
  CLI `ipn-cli rename <name>`; the GUI exposes it inline (pencil).
- **Header shows the network + state.** The title bar now displays the current network name and a
  "N device(s) · connected/disconnected" subtitle (or the offline/mismatch state).
- **Ctrl+Q** quits Nullgate (disconnect + exit), same as the tray's "Quit Nullgate".
- **Friendlier first-run.** The empty screen now has **Create / Join buttons** right on it (no
  hunting for the + menu), and a **"Connecting…"** placeholder shows until the first status
  arrives.
- **`--version`** on `nullgate`, `ipn-cli`, and `ipn-daemon`; **`--minimized`** (or
  `NULLGATE_START_MINIMIZED`) launches the GUI straight to the tray, for launch-on-login.
- **Diagnostics view.** A collapsible "Diagnostics" section on the main screen shows this device's
  home relay, a direct-vs-relay connection summary, and TUN routing state. `NetworkStatus` gained
  `home_relay`.
- **Friendly device label.** Each member can set an optional friendly name for itself (GUI
  "Set this device's name" pencil on the This-device row; CLI `set-name [name]`), broadcast over
  signed presence. The real hostname and virtual IP are always shown alongside it. `MemberView`
  gained `label`; `NetworkStatus` gained `self_label`.
- **Pending join-requests panel.** Join requests now persist in a panel at the top of the main
  window (with Approve/Deny + the emoji code) instead of a one-shot dialog, so a missed or
  dismissed prompt can still be acted on; a desktop notification fires when one arrives, and
  entries clear once the device becomes a member.
- **GUI polish.** An **About** dialog (version/license) in a new header menu; the window now
  **remembers its size**; **success toasts** for copy/remove/freeze actions; the **Join ticket
  field is validated** (must look like `ng1…`); a **copy button for this device's node ID**;
  **tooltips + a legend** explaining the online dot and direct-vs-relay; relative **"last seen"
  updates live**; and a desktop **notification when a member comes online**.
- **Originator key backup & recovery.** The originator can export its master key as a single
  `ngkey1…` recovery code (GUI "Back up originator key" → QR + copy, with a keep-it-safe
  warning; CLI `export-key`). Another member of the same network can import it ("Restore
  originator access…" / CLI `import-key`) to regain admin powers after device loss — a code for a
  different network is rejected. New `originator_key_e2e` smoke test.
- **System tray** (Open Nullgate / Quit Nullgate) with minimize-to-tray: closing the window hides it to
  the tray (keeps the connection) and notifies once; "Quit Nullgate" disconnects from the network
  locally, then exits. tray-icon on Windows/macOS, ksni on Linux; uses `img/trayicon.png`.
- **Connect / Disconnect** (engine `set_online`) so quitting takes the device offline while
  keeping the saved network; reopening the app reconnects. Wired through IPC/daemon/CLI;
  `NetworkStatus` gained `online`.
- **App icon**: embedded in the Windows `.exe` (from `img/icon.ico`) and installed into the
  Linux icon theme (`img/icon.png`); image assets added under `img/`.
- **GPL-3.0 `LICENSE`** with a §7 "Wintun exception"; bundles ship the project + Wintun licenses.
- Documentation framework: user-facing `README.md`, `CLAUDE.md` agent guide, `docs/`
  (`architecture`, `security`, `building`, `releasing`, `development`), `CHANGELOG.md`, `TODO.md`.

## [0.0.1-test5]
### Added
- **Rotate secret (re-key)** — originator-only mass-revoke: boots all members and restarts the
  network under a fresh secret, returning a new ticket. Locks out anyone with the old ticket,
  including a device that was offline during a removal.
- **Self-eviction** — a device removed from the roster (remove/delete/rotate) auto-leaves: drops
  its connections and clears the dead network.
- `rotate_e2e` smoke test.

## [0.0.1-test4]
### Added
- **Delete network** (originator dissolves the pool) and **Leave network** (per-device).
- `delete_e2e` smoke test (3 nodes).
### Fixed
- **Ghost connections**: the mesh now continuously enforces membership, tearing down a
  connection to any peer that is no longer a member.
### Changed
- Ticket dialog shows a fixed-size QR image + a compact copy box (no more screen-filling key);
  SAS emojis rendered large.

## [0.0.1-test3]
### Added
- **No-elevation UX**: split into a privileged `ipn-daemon` (owns iroh + TUN) and an
  unprivileged `ipn-gui` IPC client, plus `ipn-ipc` and `ipn-cli`. Windows service install;
  Linux runs the daemon via `setcap`.

## [0.0.1-test2]
### Changed
- Ticket dialog gained a QR code + copy button.
### Fixed
- Linux: run the GUI as the normal user with `setcap` for routing (no more `sudo` breaking the
  display).

## [0.0.1-test1]
### Added
- First testable build: create/join a network with emoji SAS verification, web-of-trust
  approval, originator remove/freeze, member list with presence, and TUN routing so RDP/SSH work
  over the virtual LAN. Windows + Linux desktop builds.
