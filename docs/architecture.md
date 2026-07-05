# How it works

Nullgate is a peer-to-peer virtual LAN built on [iroh](https://www.iroh.computer). There are no
accounts and no central coordinator — devices find and authenticate each other directly, and
the membership list is a small signed document every member replicates.

## Connectivity
- Each device has an **iroh identity**: a NodeId, which is an ed25519 public key. Connections
  are QUIC, end-to-end encrypted, and mutually authenticated by construction — the key *is* the
  identity, so a peer can't be impersonated.
- Every mesh/join handshake first exchanges a **protocol version**
  (`admission::PROTOCOL_VERSION`); a mismatch is rejected with a clear error on both ends. The
  GUI↔daemon IPC is likewise versioned (an `IpcRequest::Hello` check).
- Members form a **full mesh** of authenticated connections. iroh does NAT hole-punching for
  direct links and falls back to a relay only when a direct path can't be established. (n0 runs
  free public relays; self-hosting is on the roadmap.)
- A periodic **maintenance tick** reconciles the mesh: it rebuilds the roster, tears down
  connections to non-members, and dials any member we aren't yet connected to. Dialing is
  **de-duplicated and time-bounded** (`engine::spawn_dials`) — at most one in-flight `connect()`
  per peer, each capped by `DIAL_TIMEOUT`, with the slot freed on completion/timeout. This matters
  because an unreachable member is retried on every tick indefinitely; without the guard those
  attempts (and their iroh connection/path state) accumulated without bound.
- Each member's virtual IP on the `10.99.0.0/24` subnet is **derived deterministically from its
  NodeId** during the roster fold (so every node computes the same collision-free map and no two
  members can be handed the same address, even on simultaneous approvals). A **TUN interface**
  turns that into a real network device: outbound IP packets are matched to the destination
  member and sent as QUIC datagrams; inbound datagrams are written back to the TUN. The MTU is
  clamped (1280) and TCP **MSS clamping** is applied to SYNs (both directions) so TCP flows never
  produce segments too large for a datagram. That's why ordinary RDP/SSH/etc. clients
  work unchanged — to them it's just another network.

## Network identity
A network has a single **secret**, carried in the join ticket. Everything else is derived from
it (via HKDF), so every member can independently arrive at the same values with no coordinator:
- a **rendezvous** key used for private peer discovery (outsiders can't find the network),
- an admission **pre-shared key (PSK)** proven during the per-connection handshake,
- the **iroh-docs namespace** that holds the membership roster.

There is also a separate, exportable **originator master key** — the super-admin authority for
removals/freeze/rotate. Only its public half travels in the ticket; you can back up the private
half and re-import it on a new device.

## Membership roster
The roster is an append-only set of **signed entries** (`ipn-roster-v2`: `Add` / `Remove` /
`SetRole` / `SetInvite` / `Freeze` / `SetName`) stored in an
[iroh-docs](https://github.com/n0-computer/iroh-docs) document — a replicated multi-writer store
that every member syncs. Each node folds the entries into the current membership by applying role
rules. Each member carries a **role** (`Peer` / `Controller`); join **invites** are nonces set by
`SetInvite` and cited by the admitting `Add` (so regenerating an invite invalidates the old code).
See [security.md](security.md) for the full trust model.

**Static virtual IPs.** A member's `10.99.0.x` address is chosen by the admitter (lowest free
host) and **recorded in its `Add`**. The fold assigns IPs in admission order, honoring each
member's recorded address and probing forward only on a genuine collision — so a device keeps its
address for the life of its membership and another device joining or leaving never shifts it. (It
only changes if the device leaves and rejoins.)

**The activity log** is a 30-day, human-readable **view derived from the signed roster history**
(each entry's signer, op, and timestamp) — no separate store, so it's tamper-evident and identical
for every member. Visible to all tiers.

Presence (who's online, hostname, last-seen, and the access-disabled / hidden flags) is broadcast
separately over [iroh-gossip](https://github.com/n0-computer/iroh-gossip) on the private rendezvous
topic, each heartbeat **signed** by the device. The **hostname** is the device's *actual current*
OS hostname (re-read on every heartbeat); the public IP shown for a peer is the address your node
actually observes for it (so a peer can't spoof its own).

**One-way "disable remote access."** `conntrack.rs` tracks the flows this device initiates (on the
outbound TUN→mesh path); when the block (or hide) is on, the inbound path admits only return
traffic for a tracked flow. The toggle is an `AtomicBool` on the engine's `Inner` (read lock-free
per packet, never behind the async state mutex), persisted in `device_prefs.cbor`.

## Reliability: memory watchdog + presence-blip debounce
**Memory watchdog (iroh #4293 stopgap).** iroh 1.0's per-remote mapped-address cache
(`socket::mapped_addrs::AddrMap`) is never pruned — every distinct transport address it sees mints a
permanent entry in two `FxHashMap`s — so under address churn the daemon's resident memory grows
without bound until an allocation aborts the process (the captured minidump was a single ~80 GB
request → `0xc0000409`). Those maps live inside the iroh node, which `Engine::start` builds once and
never rebuilds (`set_online` does not recreate it), so only a **process restart** reclaims them.
`ipn-daemon/src/watchdog.rs` samples the daemon's own RSS every 30 s and, past a limit (default
1024 MB; override `NULLGATE_MEM_LIMIT_MB`, `NULLGATE_MEM_CHECK_SECS`; `0` disables), records the
reason to the crash log and exits with code 92 so the service manager (SCM failure actions / systemd
`Restart=on-failure` / launchd `KeepAlive`, all already configured for crash recovery) restarts it —
bounding memory far below the abort. Remove once
[iroh#4293](https://github.com/n0-computer/iroh/issues/4293) ships an eviction fix.

**Presence-blip debounce.** A watchdog restart (or any brief drop) makes a device flap
offline→online within seconds, which every *other* machine's daemon observes — and would otherwise
turn into a "came online" notification each time. The tray agent (`notify_newly_online` in
`ipn-gui/src/notify.rs`) tracks, per peer,
when it first went dark this session and only announces a return once the absence has exceeded a
threshold (default 2 minutes; `NULLGATE_ONLINE_DEBOUNCE_SECS`), so routine restarts stay silent
while a genuine reconnection still notifies.

## Components (crates)
- `ipn-core` — the engine: iroh node, signed roster, admission + emoji verification, presence,
  and TUN routing. UI- and IPC-agnostic (also the basis for a future Android build).
- `ipn-ipc` — the contract + transport between the GUI and the daemon (a length-prefixed CBOR
  protocol over a named pipe on Windows / a Unix socket on Linux/macOS).
- `ipn-daemon` — the **privileged** part: owns the iroh node + TUN and serves the GUI over IPC.
  Runs as a LocalSystem service on Windows, or with `CAP_NET_ADMIN` (via `setcap`) / systemd on
  Linux.
- `ipn-gui` — **Nullgate**, the GTK4 + libadwaita desktop app (binary `nullgate`). **Unprivileged** —
  it only talks to the daemon, so day-to-day use never needs admin/root. ("Nullgate" is the
  product name shown in the UI and docs; `ipn-gui` remains the codebase codename.) The same binary
  has a second, **headless mode** — `nullgate --agent`, the **tray agent** (`ipn-gui/src/agent.rs`):
  it owns the system tray + desktop notifications, and launches the GUI window on demand. See
  *Why the tray is a separate agent* below.
- `ipn-cli` — a small headless client (status / create / join / approve / remove / rotate …),
  handy for scripting and testing.
- `ipn-mobile` — the **Android** facade: a UniFFI `cdylib` (`ipn_mobile`) that runs `ipn-core`
  **in-process** behind a `MobileEngine` object + `EventListener` callback. No daemon, no IPC.

### Why the daemon/GUI split
Creating the virtual network interface needs elevated privilege; a GUI does not. Splitting them
means the privileged work is isolated in a tiny background service while the app you click runs
as you — so you elevate once at install time, never per launch.

The one exception is (re)starting the service when it's stopped or degraded — offered both by the
GUI's status banner and by the tray agent's **Restart Nullgate daemon** item. The unprivileged app
can't talk to a dead daemon and can't restart a privileged service on its own, so it raises the OS's
own graphical elevation prompt. On **Windows** it UAC-elevates the (code-signed) daemon binary
directly — `nullgate-daemon.exe restart` (a subcommand that stops, waits for Stopped, then starts)
via `ShellExecuteExW`'s `runas` verb — so **no PowerShell/`sc.exe`** is involved and the consent
dialog shows the *Nullgate* publisher. On **Linux** it's polkit (`pkexec systemctl restart …`), and
on **macOS** the auth dialog (`osascript … with administrator privileges` → `launchctl kickstart`).
This is a one-shot elevated helper (`ipn-gui/src/service_ctl.rs`); the app never holds privilege, and
its 2-second reconnect loop clears the banner once the daemon is back.

### Why the tray is a separate agent
The persistent, always-there part of Nullgate is the **daemon** — it owns the network. Yet the
daemon is a *system service* (Windows session 0, a root systemd unit, a macOS LaunchDaemon), and a
system service is walled off from the user's graphical session: it **cannot draw a tray icon or post
a notification you'd see**. So the tray can't live in the daemon. It also shouldn't live in the GUI
window — tying it there means a resource-heavy GUI must run hidden at all times, and the tray
disappears the moment the GUI is closed or crashes (misleading, since the network is still up).

The tray therefore runs in a **third process**: a lightweight, unprivileged **tray agent**
(`nullgate --agent`) that autostarts in the login session (per-user Run key / LaunchAgent / XDG
autostart), subscribes to the daemon over the same IPC socket the GUI uses, and:

- owns the **tray icon** (`ipn-gui/src/tray.rs` — `tray-icon` on Windows/macOS, `ksni` on Linux)
  and all **desktop notifications** (`ipn-gui/src/notify.rs`), so alerts fire even with the GUI
  closed and the tray survives a GUI crash;
- launches the **GUI window** on demand (tray *Open Nullgate*, or a notification click). The GUI is
  a single-instance GApplication, so re-launch just presents the existing window;
- offers **Restart Nullgate daemon** (the same elevated helper the GUI's banner uses) and **Quit
  Nullgate** (disconnect, then quit the agent).

The GUI is now a normal window: closing it quits the GUI process only; the daemon and agent keep
running. The agent uses a distinct GApplication id (`…Nullgate.Agent`) so it and the GUI can both be
primary instances at once; on Windows it registers the same AppUserModelID for toast attribution.

### Android (no daemon; VpnService)
Android has no separate-privileged-process model and won't let an app open a TUN directly, so the
desktop daemon/GUI split doesn't apply. Instead the Kotlin/Compose app (`android/`) runs the same
`ipn-core` engine **in its own process** via `ipn-mobile`, inside a foreground `VpnService`. The
control flow is inverted versus desktop: the engine can't open the TUN, so when its virtual IP is
known it emits `EngineEvent::TunSetupRequired`; the app stands up the `VpnService`
(`addAddress(10.99.0.x/24)`, `addRoute(10.99.0.0/24)`) and passes the resulting fd back to
`Engine::attach_tun_fd`, which adopts it (`tun_device::RealTun::from_fd`) and runs the exact same
outbound/inbound pump as desktop. It's a **split tunnel** — only the `10.99.0.0/24` is routed — so
the phone's normal traffic (and iroh's own relay/peer traffic to public IPs) bypasses it, which is
also why no `VpnService.protect()` is needed. See `docs/android-packaging.md`.
