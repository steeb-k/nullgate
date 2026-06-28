# TODO / ideas

A catch-all for future work. Loosely grouped and lightly prioritized. When you finish an item,
move it into a release and add a `CHANGELOG.md` entry (see `docs/development.md`).

Legend: **★ recommended next** · ⚠️ known gap/risk in the current code · 💡 idea.

## ★ Recommended next (short list)
1. ⚠️ **Fix the virtual-IP assignment race** (see Known issues) — correctness bug.
2. ⚠️ **Store secrets in the OS keystore**, not plaintext files — at-rest security.
3. **Originator key backup & recovery** (export/import recovery phrase) — the design promises a
   portable master key, but it currently only lives in one device's config.
4. **Self-host relay setting** — independence/privacy; you flagged this early.
5. **"Connect" button per member** that launches the platform RDP/SSH client at the peer's IP —
   biggest UX win for the actual use case.

## Known issues / risks to investigate
- ⚠️ **Virtual-IP assignment race.** Each approver picks `next_free_ip()` from *its own* roster
  view, so two members approving two joiners at the same time can hand out the **same IP**,
  giving two members the same address and breaking routing for one. Options: originator-
  authoritative IP assignment; derive IP deterministically from the NodeId (hash into the /24
  with rehash-on-collision); or detect a collision when folding the roster and resolve it
  deterministically (lower NodeId keeps the IP). Add an e2e test for concurrent joins.
- ⚠️ **Secrets at rest are plaintext.** `node.key`, the network secret, and the originator
  master key live in files under the data dir. Move them to the OS keystore (Windows DPAPI /
  Credential Manager, macOS Keychain, Linux Secret Service via the `keyring` crate) with a
  file fallback, mirroring seed-sync.
- ⚠️ **Roster ordering trusts wall-clock timestamps.** Entries are ordered by `ts` (then content
  hash). A member could backdate/forward-date `ts` to influence ordering (e.g. around a
  freeze/remove). Investigate logical clocks / a signed monotonic version, and clamp/validate
  timestamps. Clock skew between honest machines is the benign version of the same problem.
- ⚠️ **Doc can be spammed.** Any holder of the network secret can append entries to the iroh-docs
  replica; a malicious member could bloat it. Consider roster compaction/snapshots, entry caps,
  and pruning superseded entries. (Rotate is the blunt reset.)
- ⚠️ **Oversized packets are dropped.** Datagrams above the negotiated max are dropped (we clamp
  TUN MTU to 1280 and rely on PMTUD). Consider TCP MSS clamping, or stream fallback for large
  packets, and verify behavior with real workloads.
- ⚠️ **Protocol has no version negotiation.** ALPNs are `ipn/mesh/0` / `ipn/join/0` and the IPC
  is unversioned. A daemon/GUI or peer-to-peer version mismatch will fail confusingly. Add an
  explicit protocol version in the handshakes and the IPC hello, with a clear error.

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
- Per-member quick actions: **Connect (RDP/SSH)** launcher, copy address (done), rename.
- A diagnostics/status view: relay-in-use, direct-vs-relay (shown), throughput, NAT info; expose
  the existing `conn_probe` logic in-app.
- Tray icon + minimize-to-tray; launch on login; app icon + desktop integration.
- Friendlier first-run / onboarding; clearer empty and error states.

## Platforms
- macOS packaging (notarized `.app` or `curl|sh`; daemon as launchd / Network Extension).
- Android: Kotlin/Compose UI over a UniFFI facade around `ipn-core`, TUN via `VpnService`.

## Packaging / installers / ops
- Real installers (see `docs/releasing.md` "Planned installers"): Windows MSI + code signing,
  Linux `.deb`/AppImage/Flatpak + a systemd unit, macOS notarized app, Android APK.
- Auto-update mechanism (and a way to view/rotate logs).
- Log to a file with rotation; a "view logs" affordance in the GUI.

## Testing & tooling
- A one-shot test runner script (unit + all ignored e2e) for local pre-release checks.
- More e2e: concurrent-join IP race, 5+ node scaling, reconnect after a network blip,
  offline-during-removal rejoin, mDNS-only (no internet).
- Property/fuzz tests for the roster fold against adversarial entry sets.
- Wire `cargo clippy` + `cargo fmt --check` into the dev workflow; clear the existing GUI warning.

## Nice-to-haves / quick wins (low effort)
Polish and small ergonomics — likely an afternoon each or less.
- `--version` on `ipn`, `ipn-daemon`, `ipn-cli`; show the version in the GUI (header/About).
- An **About** dialog (version, repo link, license, credits).
- Clear the existing `ipn-gui` compiler warning; add a `cargo fmt`/`clippy` clean pass.
- Toasts confirming actions ("Ticket copied", "Member removed", "Network frozen").
- Validate the Join ticket field (reject input that isn't an `ipn1…` ticket, with a clear hint).
- Disable a button while its action is in flight (avoid double-submits).
- Remember window size/position between runs.
- Show this device's own NodeId somewhere + a copy button (useful for `add-key`/debugging).
- App/window icon; a Linux desktop icon (the `.desktop` exists, ship an icon with it); a
  Windows Start-menu shortcut in the bundle.
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
