# Changelog

All notable changes to IPN. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]
### Changed
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
