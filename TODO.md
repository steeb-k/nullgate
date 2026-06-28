# TODO / ideas

A catch-all for future work. Loosely grouped and lightly prioritized. When you finish an item,
move it into a release and add a `CHANGELOG.md` entry (see `docs/development.md`).

Legend: **★ recommended next** · ⚠️ known gap/risk in the current code · 💡 idea.

## ★ Recommended next (short list)
IPN is a **general-purpose ad-hoc VPN** (not RDP-specific), so current focus is **GUI &
usability** polish — see the "UX / product" and "Nice-to-haves / quick wins" sections.

- **Self-host relay setting** — independence/privacy. Kept in **planning** (not urgent while
  n0's public relays work).

(Done: virtual-IP race → deterministic IPs; secrets → OS keystore; originator key backup &
recovery → export/import recovery code in GUI + CLI. Dropped: per-member RDP/SSH "Connect"
launcher — out of scope now that IPN is a general VPN.)

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
  machine/user; `IPN_SECRETS_FILE_ONLY=1` forces the file backend, used by tests.)
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
- IPv6 inside the virtual LAN (IPv4-only today); larger/!/configurable subnet than a single /24.

## UX / product
- Editable device name in the UI (instead of the raw hostname).
- A **pending join requests** panel — today the approval is a transient dialog; if dismissed or
  missed there's no way to re-approve.
- Desktop notifications (join request received, member came online).
- Per-member quick actions: copy address (done), rename. (Dropped the RDP/SSH launcher — IPN is a
  general VPN, so users point their own tools at the peer IP.)
- A diagnostics/status view: relay-in-use, direct-vs-relay (shown), throughput, NAT info; expose
  the existing `conn_probe` logic in-app.
- Friendlier first-run / onboarding; clearer empty and error states.

(Done: app icon, system tray, minimize-to-tray, "Quit IPN" disconnects then exits.)

## Platforms
- macOS packaging (notarized `.app` or `curl|sh`; daemon as launchd / Network Extension).
- Android: Kotlin/Compose UI over a UniFFI facade around `ipn-core`, TUN via `VpnService`.

## Packaging / installers / ops
- Real installers (see `docs/releasing.md` "Planned installers"): Windows MSI + code signing,
  Linux `.deb`/AppImage/Flatpak + a systemd unit, macOS notarized app, Android APK.
- **Launch on login (autostart), starting minimized to the tray** — register from the installer
  (Windows: Run key / Startup shortcut; Linux: XDG autostart `.desktop`; macOS: LoginItem).
  Needs the GUI start-hidden flag (see quick-wins).
- A Windows Start-menu shortcut in the bundle.
- Auto-update mechanism (and a way to view/rotate logs).
- Log to a file with rotation; a "view logs" affordance in the GUI.

## Testing & tooling
- A one-shot test runner script (unit + all ignored e2e) for local pre-release checks.
- More e2e: 5+ node scaling, reconnect after a network blip, offline-during-removal rejoin,
  mDNS-only (no internet). (Concurrent-join IP race is covered by roster unit tests.)
- Property/fuzz tests for the roster fold against adversarial entry sets.
- Wire `cargo clippy` + `cargo fmt --check` into the dev workflow; clear the existing GUI warning.

## Nice-to-haves / quick wins (low effort)
Polish and small ergonomics — likely an afternoon each or less.
- `--version` on `ipn`, `ipn-daemon`, `ipn-cli`; show the version in the GUI (header/About).
- An **About** dialog (version, repo link, license, credits).
- A `cargo fmt` / `clippy` clean pass.
- Toasts confirming actions ("Ticket copied", "Member removed", "Network frozen").
- Validate the Join ticket field (reject input that isn't an `ipn1…` ticket, with a clear hint).
- Disable a button while its action is in flight (avoid double-submits).
- Remember window size/position between runs.
- Show this device's own NodeId somewhere + a copy button (useful for `add-key`/debugging).
- GUI start-hidden / `--minimized` flag (lets autostart launch straight to the tray).
- Dev convenience scripts: `scripts/run-dev` (start daemon + GUI) and `scripts/test`
  (unit + all ignored e2e) for quick local checks.
- A short top-level `CONTRIBUTING.md` that points at `docs/development.md`.
- A relative "last seen" that updates live without needing a roster change.
- Tooltips/legend for the status dot and direct-vs-relay labels.

## Maybe / ideas
- 💡 More than one network per device at once.
- 💡 Per-peer "last seen" history / connection-quality graph.
- 💡 Optional headless/server mode (a member that's just a reachable host, no GUI).

---

The original feasibility goals are **implemented**: a private virtual LAN over iroh linking your
own devices, reachable by stable private IPs with existing clients, no full-tunnel VPN chokepoint,
and simple access control (add a device key + a network password, with remove/rotate to block
anyone who previously had access). Building a full custom RDP client was intentionally **not**
pursued — IPN provides the network; you use the RDP/SSH/etc. clients you already have.
