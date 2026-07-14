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
  direct links and falls back to a relay only when a direct path can't be established. n0 runs
  free public relays; a device can instead use **custom relay servers** (self-hosted iroh relays,
  `ipn-core/src/relays.rs`), configured per device in `relays.cbor` with an optional per-relay
  access token (sent as `Authorization: Bearer` on the relay handshake; the relay rejects clients
  without it). `RelaySettings::desired_relay_configs()` is the single source of the relay map, used
  both at bind (`node.rs`) and on a live edit. Two policies:
  | policy | relay map | effect |
  |--------|-----------|--------|
  | **preferred** (default) | custom **+** the public defaults | your relay carries the traffic, but you can still reach — and be reached by — peers that don't have it |
  | **only** | custom alone | third-party infrastructure is never contacted; peers without the relay (or its token) cannot reach you |

  A custom `PathSelector` (`relays::PreferMyRelaySelector`, installed at bind via iroh's
  `unstable-custom-transports` feature) mirrors iroh's default biased-RTT path choice but ranks a
  path through one of the *user's* relays (tier 1) above any other relay (tier 2); direct paths
  (tier 0) still always win. Under `preferred` the map genuinely holds both tiers, which is the case
  the selector exists for. The preferred-set holds the **custom URLs only**, never the defaults that
  share the map with them.

  **An endpoint advertises exactly one relay — its home relay** — picked by latency from the map
  (`Endpoint::addr()` carries a single relay URL). So the map is what *we* can reach peers through;
  the home relay is what peers can reach *us* through. That asymmetry is the whole reason a
  partially-deployed token-gated relay partitions a network, and why `preferred` keeps the public
  relays: without them a configured device has no transport that can reach a peer homed on a public
  relay, and the peer can't reach its token-gated one either — mutually invisible, with the relay
  perfectly healthy. (This replaced a `relay_watchdog` that watched `home_relay_status()` — i.e.
  "can *I* reach my relay", which was always yes. It could never observe "my peers can't reach my
  relay", so it never fired. It is gone.)

  Settings changes are saved, swapped in memory, and returned **immediately**; the new map is pushed
  into the live endpoint by a serialized background task (`engine::apply_relay_map`), whose progress
  is reported as `RelayApply::{Applied,Pending,Failed}` on `GetRelays`. It must be off the request
  path: `Endpoint::insert_relay`/`remove_relay` await iroh's bounded socket-actor channel, and that
  actor can block indefinitely behind a peer stuck sending to an unreachable relay — see the
  gotchas in `CLAUDE.md`. No daemon restart is needed, but we verify that rather than assert it
  (`engine::settle_home_relay`), because iroh keeps a home relay that has left the map until another
  relay takes over.

  A relay (and its token, if any) can be **checked before it is saved**: `relays::probe_relay()`
  binds a throwaway endpoint whose map holds nothing but that relay and waits for it to come online,
  which is the only way to ask the question — the token rides on the websocket upgrade and the
  relay's access check runs *after* it, so a rejected client gets the same `101` as anyone else and
  is dropped inside the stream. It never touches the running endpoint, so it can't wedge the socket
  actor. Exposed as `IpcRequest::ProbeRelay`, and used by both clients: `nullgate-cli relay add`
  refuses a token the relay won't take, and the GUI tests a relay before saving it (in-dialog, with
  an **Add anyway** escape) and again on demand from each row. A wrong token and an unreachable relay
  are indistinguishable from outside, and the error says so.

  The settings are deliberately **not** distributed through the roster: every member configures its
  own. That is a real hazard — see the security note — so both UIs warn about it.
- **UI change events are gated and coalesced.** The engine emits a `Changed` event only when
  something user-visible actually changed (the presence tracker's mutators report whether they
  changed displayed state; the maintenance tick keeps a dirty flag, with an unconditional event
  every ~30 s as a catch-all for live-read fields like the home relay). The daemon then coalesces
  bursts into one `status()` + IPC push per subscriber after a 250 ms quiet window — join-approval
  events are forwarded immediately and never collapsed. The GUI applies each status **in place** to
  a build-once widget tree (member rows diffed by node id; reordering re-sorts a `gtk::ListBox`
  without destroying rows), so a status push can never destroy a widget mid-click. Before this,
  every heartbeat emitted an event (N members ≈ N events/3 s), every event pushed a status, and any
  visible change rebuilt the entire page — which is what made clicks feel like they didn't work.
- A periodic **maintenance tick** (every 3 s) reconciles the mesh: it rebuilds the roster, tears
  down connections to non-members, and dials any member we aren't yet connected to. Dialing is
  **de-duplicated and time-bounded** (`engine::spawn_dials`) — at most one in-flight `connect()`
  per peer, each capped by `DIAL_TIMEOUT`, with the slot freed on completion/timeout. This matters
  because an unreachable member is retried on every tick indefinitely; without the guard those
  attempts (and their iroh connection/path state) accumulated without bound.
- **One connection per peer, deterministically.** When both ends dial each other at once (common
  right after any drop, since both tick every 3 s) two connections briefly exist. A tie-break
  (`engine::resolve_duplicate`) makes *both* sides keep the **same** one — the connection initiated
  by the lower NodeId — and cleanly close the other; a re-dial from the same side keeps the newer
  conn, and an already-dead entry is always replaced so a restarted peer reconnects at once. The
  close-watcher only evicts a peer's map entry when the connection that closed is still the live one
  (guarded by iroh's `stable_id`), so a superseded duplicate closing can't drop the live link — that
  race previously caused unexplained per-peer drops.
- **Connection lifecycle is logged** (at the daemon's default `info` level): mesh connections log
  when they're **established** (with direction), **replaced** by a duplicate, and **closed** —
  the last carrying the QUIC `ConnectionError` reason (`warn` for an unexpected loss, `info` for a
  deliberate close). This makes an intermittent drop attributable from the log instead of silent.
- **Swarm re-seeding is throttled to reachable members.** The roster-doc live-sync swarm and the
  presence-gossip mesh both need periodic re-seeding to stay connected, but each attempt *dials* the
  target — and dialing **unreachable** members on a timer was minting permanent entries in iroh's
  mapped-address cache (see the watchdog note below), the churn that drove the restart loop behind
  the intermittent drops. So: a **membership change** re-seeds everyone immediately (keeps
  removals/additions propagating within seconds), while the periodic self-heal re-seeds only members
  we believe are reachable — a live mesh conn or a fresh presence heartbeat (`engine::doc_reseed_targets`,
  `DOC_RESYNC_MS` = 30 s) — and gossip `join_peers` runs on change or a 60 s cadence (15 s while we
  have zero neighbors). The 3 s presence *broadcast* is unchanged: it only reaches current neighbors
  and never dials.
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

## Reliability: memory watchdog + presence-blip debounce + sleep/wake
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
[iroh#4293](https://github.com/n0-computer/iroh/issues/4293) ships an eviction fix. The
reachable-only re-seed throttle in the maintenance tick (above) attacks the *cause* — it stops the
daemon from re-dialing unreachable members every few seconds, which is what fed those permanent
address-map entries — so the watchdog should trip far less often; the watchdog stays as the backstop
until the upstream fix lands.

**Presence-blip debounce.** A watchdog restart (or any brief drop) makes a device flap
offline→online within seconds, which every *other* machine's daemon observes — and would otherwise
turn into a "came online" notification each time. The tray agent (`notify_newly_online` in
`ipn-gui/src/notify.rs`) tracks, per peer,
when it first went dark this session and only announces a return once the absence has exceeded a
threshold (default 2 minutes; `NULLGATE_ONLINE_DEBOUNCE_SECS`), so routine restarts stay silent
while a genuine reconnection still notifies.

**Sleep/wake (`ipn-daemon/src/power.rs`).** The debounce above cannot help a *suspended* device: it
has been dark for hours, so every reconnection reads as genuine. And a suspended device reconnects
far more often than one might expect — macOS schedules a **dark wake** (a brief maintenance wake,
courtesy of Power Nap, and entirely independent of the "wake for network access" setting) every few
minutes on battery. The daemon is frozen, not stopped, so each dark wake resumed it, re-established
the mesh for a couple of seconds, and made every *other* device in the pool announce "came online".

So the daemon follows the machine's power state: it calls `Engine::set_online(false)` before the
system sleeps — peers get a clean QUIC close instead of an idle timeout — and `set_online(true)`
again only on a **full wake**. Dark wakes are ignored, which is also honest: nothing can reach a
laptop that is seconds from sleeping again. A device the user had already disconnected by hand is
left alone, since `power.rs` only restores what it took down (`resume_on_wake`).

The policy in `power.rs` is platform-free; the backend is not. macOS uses `IOPMConnection`
(`power/macos.rs`), because the documented `IORegisterForSystemPower` reports a dark wake and a real
wake identically — `IOPMConnection` instead hands the callback the system *capability* bits, and a
wake without the graphics bit is a dark wake. Windows' Modern Standby (S0) has the same problem and
would hook `PowerRegisterSuspendResumeNotification`; Linux would take a logind sleep inhibitor.
Neither is implemented. `NULLGATE_DISABLE_POWER_EVENTS=1` turns the whole thing off.

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

### Per-device action buttons (the one thing the daemon doesn't know)
A member can be given a labelled, coloured button that runs a command — `mstsc /v:{ip}`, `ssh
me@{ip}` — shown on its row in the member list and in the tray menu. It is implemented entirely in
`ipn-gui` (`src/actions.rs`), with **no engine, IPC, or protocol involvement**, which makes it the
only piece of per-member state that doesn't come from the daemon. Both departures from the usual
layering are deliberate:

- **It is local to the machine, not to the network.** The command that reaches a device is a
  property of the computer you're sitting at, so distributing it through the signed roster would
  push a Windows command line onto a Mac. Nicknames and notes are local too — but they are stored
  by the *daemon* (`nicknames.cbor`, `notes.cbor`) and delivered in `MemberView`, which is exactly
  what this does not do.
- **It is an executable command line.** The daemon runs as SYSTEM/root and its IPC socket is
  reachable by every local user. Storing exec strings there — to be spawned later by whichever
  user's GUI reads them back — would turn an inert local IPC surface into a cross-user
  code-execution path. A note is text; this is not. So it lives in the *user's own* config
  directory (`actions.json`, alongside the window-size file), writable only by the user whose GUI
  will run it, and never crosses the IPC boundary. See `docs/security.md`.

Commands are spawned directly (`std::process::Command`), never through `cmd.exe`/`sh -c`: the line
is split on whitespace with double quotes grouping and **no backslash escapes** (so `C:\Program
Files\…` survives), and each resulting token is placeholder-expanded individually — expanding
*after* the split is what stops a device named `Media Box` from silently becoming two arguments.

A button may ask to **run in a terminal window**, which is the difference between `ssh` working and
not. The default spawn is detached with `Stdio::null()` on all three streams — correct for a
graphical program, useless for one that wants a console — so the terminal path is separate per
platform:

| | how |
|---|---|
| **Windows** | `CREATE_NEW_CONSOLE`, and **no stdio redirection at all**. That second half is load-bearing: a GUI-subsystem process has no standard handles, so Rust omits `STARTF_USESTDHANDLES` and Windows wires the child's streams to the fresh console. Pass `Stdio::null()` and the console window still appears — but the program is talking to `NUL`, so it looks hung. |
| **Linux/BSD** | the first terminal emulator found on `PATH` (or `$TERMINAL`), from a table that records *each one's* "the rest is the command" flag — `-e` is not universal, and `gnome-terminal -e` takes a single string and would drop every argument after the program. |
| **macOS** | Terminal.app takes a *file*, not an argv, so a throwaway `.command` script is written with each token POSIX-single-quoted and handed to `open -a Terminal`. This is the one place a shell is involved; the quoting is ours, built from the already-split argv, so a token containing `;` or `$(…)` stays one literal argument. |

The GUI and the tray agent are separate processes, so the agent re-reads `actions.json` on a 1 s
mtime poll (cheaper than a filesystem-watcher dependency) and rebuilds the tray's device section;
member *names* for those entries come from the status stream it already subscribes to.

The eight colours are the app's only **dynamic** stylesheet. The palette is one **vivid** hex per
colour and nothing else: the button draws that vivid as its 1px border and derives the interior from
it (tinted toward white in light mode, toward black in dark), while the text colour follows the
*theme* rather than the hue. Deriving it all from a single value is what keeps the hues consistent —
picking fills and text per hue is what previously left yellow as the one button that didn't match.
Since both themes are derived, and GTK4 CSS has no `@media` query while libadwaita only auto-swaps
its own named colours, the rules are regenerated on `AdwStyleManager::notify::dark`.

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
(`nullgate --agent`), subscribing to the daemon over the same IPC socket the GUI uses, that:

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

**Ensuring the agent is up.** The agent must be running for the tray to exist, so it is (re)launched
from every angle a user session offers — and, being single-instance, a redundant launch just hands
off to the running one and exits, so all of these are safe to fire unconditionally:

- **at login** — the per-user autostart entry (Windows Run key / macOS LaunchAgent / Linux XDG
  autostart) runs `nullgate --agent`;
- **when the GUI starts** — `main()` spawns `nullgate --agent` (see `spawn_agent`), so opening the
  app brings the tray up immediately even on a fresh install where autostart hasn't fired yet;
- **on install/upgrade** — the platform installers launch the agent in the user's session as their
  last step (`nullgatectl` on Linux/macOS; the SYSTEM auto-updater's user-session relaunch on
  Windows), so the tray appears without waiting for a re-login.

Note the daemon is deliberately *not* one of these triggers: it runs in an isolated system session
that can't draw UI, and the agent's lifetime is independent of it (it survives daemon restarts and
reconnects), so nothing about the daemon starting/stopping needs to touch the agent.

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
