# Changelog

All notable changes to IPN. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]
### Changed
- **About dialog cleaned up.** Shows the app icon and the name "Iroh Private Network", developer
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
- **GUI redesigned (SEED-style).** A static "IPN / Iroh Private Network" titlebar; a stylesheet
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
- **Ctrl+Q** quits IPN (disconnect + exit), same as the tray's "Quit IPN".
- **Friendlier first-run.** The empty screen now has **Create / Join buttons** right on it (no
  hunting for the + menu), and a **"Connecting…"** placeholder shows until the first status
  arrives.
- **`--version`** on `ipn`, `ipn-cli`, and `ipn-daemon`; **`--minimized`** (or
  `IPN_START_MINIMIZED`) launches the GUI straight to the tray, for launch-on-login.
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
  field is validated** (must look like `ipn1…`); a **copy button for this device's node ID**;
  **tooltips + a legend** explaining the online dot and direct-vs-relay; relative **"last seen"
  updates live**; and a desktop **notification when a member comes online**.
- **Originator key backup & recovery.** The originator can export its master key as a single
  `ipnkey1…` recovery code (GUI "Back up originator key" → QR + copy, with a keep-it-safe
  warning; CLI `export-key`). Another member of the same network can import it ("Restore
  originator access…" / CLI `import-key`) to regain admin powers after device loss — a code for a
  different network is rejected. New `originator_key_e2e` smoke test.
- **System tray** (Open IPN / Quit IPN) with minimize-to-tray: closing the window hides it to
  the tray (keeps the connection) and notifies once; "Quit IPN" disconnects from the network
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
