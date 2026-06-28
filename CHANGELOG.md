# Changelog

All notable changes to IPN. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]
### Added
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
