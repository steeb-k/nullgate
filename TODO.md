# TODO / ideas

A catch-all for future work. Loosely grouped and lightly prioritized. When you finish an item,
move it into a release and add a `CHANGELOG.md` entry (see `docs/development.md`).

Legend: **★ recommended next** · ⚠️ known gap/risk in the current code · 💡 idea.

## Release status
**0.1.0** is published (signed Windows MSI + Linux tarball; macOS pending a Mac build) as a full
release on the now-**public** repo, with auto-update wired on all platforms. Per-platform
packaging: `docs/{windows,linux,macos}-packaging.md`.

## 0.1.1 — fixes queued (from hands-on testing)
- ✅ **Windows console window** alongside the GUI — fixed: `ipn-gui` is now a GUI-subsystem binary
  in release builds (`#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem="windows")]`).
- ✅ **Friendly name didn't redraw** until the flyout closed — fixed: `set_nickname_dialog` now
  optimistically re-`fill_member`s the open detail + refreshes the list on save.
- ✅ **Two Windows tray icons** (a stray app-icon one that did nothing + our functional one) —
  fixed: GLib's `GNotification` backend was spinning up its own notification-area icon to host
  toasts. All `send_notification` now goes through a `notify()` helper that is a **no-op on
  Windows** (Linux/macOS unchanged). *Verify on Windows.* Follow-up: native **WinRT toasts** on
  Windows (needs an AppUserModelID on the MSI's Start-menu shortcut) to bring those notices back.
- ✅ **"Big issue" — joining gives full LAN access? NO leak.** Code audit + live host scan
  confirm Nullgate only routes `10.99.0.0/24` (no route to physical subnets, no IP forwarding/NAT).
  The observed "full access" was a test artifact: the test laptop's phone hotspot was bridging
  the phone's own Wi-Fi, putting it back on the home LAN, so RDP reached `192.168.x` over the
  ordinary LAN — Nullgate uninvolved. Keep the flat virtual-LAN model. (Verification method now in
  MANUAL-TESTING.md: isolate the remote box; connect to `10.99.0.x`, never `192.168.x`.)
- ⚠️ **Linux .desktop icon shows broken (magenta/black).** The generated PNGs are *valid*
  (verified with `identify`), so it's a theme-lookup/path issue, not a corrupt file. Likely the
  installed `/usr/local/share/icons/hicolor` base isn't picked up (XDG_DATA_DIRS / icon-cache /
  missing `index.theme`), or the screenshot was the *uninstalled* tarball `.desktop`. Fix:
  confirm the scenario, then ensure the icon resolves (run `gtk-update-icon-cache` on the base,
  and/or also install to a definitely-searched path, or ship a pixmaps fallback).
- 🟡 **Daemon teardown — add graceful shutdown.** Stopping the service *does* drop connectivity in
  practice (process exit removes the TUN; confirmed once the test rig was on a different network),
  so the earlier "doesn't drop" was the same-LAN artifact above. Still worth doing for
  graceful/deterministic teardown: `Engine` has no `shutdown()`/`Drop` and `IrohNode::shutdown()`
  (node.rs:129) is never called. Add `Engine::shutdown()` (abort net tasks → drop TUN →
  `endpoint.close()`) and call it from `serve()`'s shutdown branch.
- 🟡 **Inbound packet filter (defense in depth).** The inbound datagram pump writes any peer's
  packet to the TUN unchecked; drop packets whose dst isn't in `10.99.0.0/24` (and optionally
  require src == that peer's roster vIP). The OS already drops stray dsts (forwarding off), so
  this is hardening, not a live hole.

## ★ Recommended next (short list)
Nullgate is a **general-purpose ad-hoc VPN** (not RDP-specific). The known-issue hardening list is
done/mitigated and the GUI has had a big SEED-style pass. Likely next:
- **Self-host relay setting** — point at your own iroh relay (independence/privacy). Kept in
  planning; not urgent while n0's public relays work.
- **A `cargo fmt` + `clippy` clean pass** and a one-shot test runner script (`scripts/test`).
- **Robustness tests** still missing: offline-during-removal rejoin, mDNS-only (no internet),
  5+-node pools (see "Robustness").
- Then **macOS packaging** / **Android** when desktop feels solid.

## Recently shipped (this session)
Captured here since it's not all obvious from a glance — see `CHANGELOG.md [Unreleased]` for detail.
- **All 6 known issues** addressed (see below): deterministic IPs, OS-keystore secrets, MSS
  clamping, protocol versioning; timestamp + doc-spam mitigations.
- **Originator key backup & recovery** — export/import an `ngkey1…` recovery code (GUI + CLI).
- **Hostname = live OS truth** (re-read each beat, not editable); **friendly names are LOCAL
  per-client nicknames** (set for *other* members, never broadcast).
- **Public IP advertised** in the signed presence heartbeat; **Local IP / Public IP** shown.
- **Geolocation** ("City, State, Country") — originator downloads the DB-IP City DB (CC BY 4.0,
  ~60 MB, runtime, bi-weekly refresh), resolves each member's public IP, and propagates signed
  location strings; members need no DB and make no external calls. `geo` module + `geo_e2e`.
- **GUI redesign (SEED-style)**: static "Nullgate" titlebar, borrowed `style.css`/`windows.css`,
  **overlay flyouts** (`adw::OverlaySplitView`) that cover the full window; Members list at the
  bottom (this device included) with a per-member **detail flyout** (info + copy + kick);
  **color-coded status dots** (green/gray/red>1wk, last-seen persisted); **Administration** flyout
  hosts network rename + freeze/rotate/recovery/delete, with **Join requests** inside it (flashing
  red chip on the row + light-red attention area). "+" hidden when in a network; **About** is a
  row; **Ctrl+Q**, **`--version`**, **`--minimized`**, remember-window-size, toasts, ticket
  validation, tooltips/legend, friendlier empty/connecting/error states.
- **Render memoization** so the page only rebuilds on real changes (keyboard focus / clicks no
  longer stolen); **join-decline** cleanly resets the joiner to no-network.
- **New app icon** (`img/icon-spin.*`).

## Known issues / risks to investigate
- ✅ **Virtual-IP assignment race — FIXED.** IPs are no longer chosen by the approver; each
  member's IP is derived deterministically from its NodeId during the roster fold
  (`roster::assign_ips`: `2 + blake3(node_id) mod 253`, NodeId-ordered probe on collision), so
  every node computes the identical, collision-free map and concurrent approvals can't clash.
  Covered by `roster` unit tests (`ips_are_unique_in_subnet_and_deterministic`,
  `concurrent_adds_get_distinct_ips`) and the e2e distinct-IP assertions.
- ✅ **Secrets at rest — FIXED.** The device key, network secret, and originator master key now
  live in the OS keystore (`keyring`: Credential Manager / Keychain / Secret Service), with a
  `0600`-file fallback for headless hosts. A per-secret `.in-keystore` marker prevents silently
  regenerating identity when the keystore is briefly unavailable (it errors instead). The
  on-disk `network.cbor` holds only non-secret fields. (Assumes one daemon instance per
  machine/user; `NULLGATE_SECRETS_FILE_ONLY=1` forces the file backend, used by tests.)
- 🟢 **Roster ordering trusts wall-clock timestamps — mitigated, residual accepted.** Done:
  far-future timestamps are dropped (`MAX_FUTURE_SKEW_MS`), and a member can't sign an `Add`
  backdated to before its own admission. The deeper case (a *trusted* member backdating an `Add`
  into a past unfrozen window) would need causal ordering (a DAG) — **not being pursued**: it's
  out of scope for a personal network of your own devices, and remove/rotate is the backstop.
- 🟡 **Doc can be spammed — partially mitigated.** Done: the fold only considers `e/`-prefixed
  entries, skips oversized values (`MAX_ENTRY_BYTES`), and caps how many it folds
  (`MAX_ENTRIES`), so a spammed replica can't OOM/peg a member. **Residual:** a malicious member
  (who holds the doc write-cap) can still grow the on-disk replica. Fully bounding that needs
  **originator-signed roster snapshots + pruning** of subsumed entries (a real model change),
  deferred; **rotate** (fresh namespace) is the backstop, and the originator can **remove** the
  offender.
- ✅ **Oversized packets — addressed via TCP MSS clamping.** TCP SYNs (both directions) have
  their MSS clamped to `MTU-40` (`router::clamp_tcp_mss`), so TCP flows (RDP/SSH/file copy) never
  exceed the tunnel and get black-holed; oversized-datagram drops are now logged, not silent.
  Unit tests cover the clamp + checksum. (Residual: non-TCP jumbo/UDP relies on the TUN MTU and
  PMTUD; if ever needed, add ICMP "frag-needed" emission for clean PMTUD.)
- ✅ **Protocol version negotiation — FIXED.** The mesh/join handshake exchanges
  `admission::PROTOCOL_VERSION` in-band and rejects a mismatch with a clear error on both ends
  (the rejecting side finishes the stream + lingers so the peer reads it, not a bare "connection
  lost"). The GUI does an `IpcRequest::Hello` version check with the daemon and shows a "version
  mismatch" page. Covered by `protocol_version_e2e`.

## Hardening (security)
- Self-host relay setting (point at your own iroh relay).
- Reconnect / keepalive tuning (periodic keepalive datagrams to hold hole-punches; faster
  recovery after blips; backoff on failed dials instead of retrying every tick).
- Bind the emoji SAS to the QUIC TLS channel (channel binding / exporter) so it provably matches
  the live connection, not just the exchanged nonces.
- Rotate the **originator master key** itself (today only the network secret rotates).
- Expiring / one-time **invite tokens** distinct from the long-term network secret, so handing
  out an invite doesn't hand out the permanent secret. (Today the ticket *is* the secret.)
- Rate-limit / back off repeated join + mesh handshake attempts at the daemon (anti-DoS).
- Optional intra-LAN segmentation: restrict which ports/peers a member can reach (e.g. expose
  only 3389), instead of full any-to-any.
- A `SECURITY.md` / threat model, and an audit/event log of joins, removals, and rotations.

## Robustness / correctness
- Daemon supervision: systemd `Restart=always` (Linux) and Windows service recovery actions;
  verify clean restart picks the network back up.
- Reconnect/backoff and self-healing of the mesh under churn; test 5+ node pools.
- Verify the "offline during removal, then reconnect" path actually boots the device on rejoin
  (self-eviction relies on it seeing the removal — add a test).
- Offline / no-internet LAN: confirm mDNS discovery (already wired) lets two devices on the same
  network connect with no relay/Internet; add a test.
- IPv6 inside the virtual LAN (IPv4-only today); larger/configurable subnet than a single /24.
- IPv6 geolocation (today only the IPv4 DB-IP City DB is fetched; v6 peers get no Location).

## UX / product
- ~~Editable device name~~ — done as **local nicknames** (per-client, set for other members).
- ~~Pending join requests panel~~ — done (now inside Administration with a flashing chip).
- ~~Desktop notifications (join request, member online)~~ — done.
- ~~Diagnostics view~~ — done (home relay, direct/relay; throughput/NAT still TODO if wanted).
- ~~Friendlier first-run / empty / error states~~ — done (Create/Join on empty, Connecting page,
  daemon-down / version-mismatch pages).
- Per-member quick actions: copy address (done), nickname (done). (Dropped the RDP/SSH launcher.)
- Throughput / NAT-type in Diagnostics (needs daemon counters/probe) — still open.

(Done: app icon, system tray, minimize-to-tray, "Quit Nullgate" disconnects then exits.)

## Platforms
- macOS packaging (notarized `.app` or `curl|sh`; daemon as launchd / Network Extension).
- Android: Kotlin/Compose UI over a UniFFI facade around `ipn-core`, TUN via `VpnService`.

## Packaging / installers / ops
- Real installers (see `docs/releasing.md` "Planned installers"): Windows MSI + code signing,
  Linux `.deb`/AppImage/Flatpak + a systemd unit, macOS notarized app, Android APK.
- **Launch on login (autostart), starting minimized to the tray** — register from the installer
  (Windows: Run key / Startup shortcut; Linux: XDG autostart `.desktop`; macOS: LoginItem). The
  GUI start-hidden flag (`--minimized` / `NULLGATE_START_MINIMIZED`) already exists; just wire the
  autostart entry in the installer.
- A Windows Start-menu shortcut in the bundle.
- **Installer should put the daemon binary in a stable location (e.g. Program Files), not run it
  from the unpacked `dist` folder** — currently the installed service locks `dist\…\nullgate-daemon.exe`,
  so rebuilding in place fails until the service is stopped. (Local-dev annoyance.)
- Auto-update mechanism (and a way to view/rotate logs).
- Log to a file with rotation; a "view logs" affordance in the GUI.

## Testing & tooling
- A one-shot test runner script (unit + all ignored e2e) for local pre-release checks.
- More e2e: 5+ node scaling, reconnect after a network blip, offline-during-removal rejoin,
  mDNS-only (no internet). (Concurrent-join IP race is covered by roster unit tests.)
- Property/fuzz tests for the roster fold against adversarial entry sets.
- Wire `cargo clippy` + `cargo fmt --check` into the dev workflow; do a clean pass.

## Nice-to-haves / quick wins (low effort)
Remaining small ergonomics (many quick-wins are now done — see "Recently shipped").
- A `cargo fmt` / `clippy` clean pass.
- Disable a button while its action is in flight (avoid double-submits). (Largely moot — dialogs
  close on submit — but the few main-window actions could use it.)
- Dev convenience scripts: `scripts/run-dev` (start daemon + GUI) and `scripts/test`
  (unit + all ignored e2e) for quick local checks.
- A short top-level `CONTRIBUTING.md` that points at `docs/development.md`.

## Maybe / ideas
- 💡 More than one network per device at once. (Hurdles assessed: the crux is the data plane —
  non-overlapping subnets, per-network connections via the ALPN, and one-TUN-per-network vs a
  shared routing table; everything above the data plane is a mechanical single→map refactor.)
- 💡 Per-peer "last seen" history / connection-quality graph.
- 💡 Optional headless/server mode (a member that's just a reachable host, no GUI).

---

The original feasibility goals are **implemented**: a private virtual LAN over iroh linking your
own devices, reachable by stable private IPs with existing clients, no full-tunnel VPN chokepoint,
and simple access control (add a device key + a network password, with remove/rotate to block
anyone who previously had access). Building a full custom RDP client was intentionally **not**
pursued — Nullgate provides the network; you use the RDP/SSH/etc. clients you already have.
