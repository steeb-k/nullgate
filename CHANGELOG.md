# Changelog

All notable changes to Nullgate. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]

### Changed
- **`nullgate-cli relay add` no longer takes `--token`; it asks for the token.** Passing a secret as
  an argument put it in the shell's history file and in every other local user's view of `ps` — a
  leak to exactly the readers that root ownership of the data dir is meant to exclude. It is now
  read with echo off (from the terminal, or from stdin when piped, so scripting still works), and a
  blank answer means the relay has no token.

### Added
- **Per-device action buttons.** A device can be given a labelled, coloured button — "RDP", "SSH",
  "Web" — that runs a command of your choosing. It appears on that device's row in the member list
  (left of the chevron) and in a new section at the top of the tray menu, read as "*device*
  (*label*)", so a machine can be reached without opening the window. Configured from **Action
  button** in the member panel: a label, one of eight colours, a command, and a **Open in a terminal
  window** checkbox. `{ip}`, `{name}`, `{hostname}` and `{node_id}` are substituted at click time.
  The command is spawned as a program with arguments — **no shell**, so a config file can't smuggle
  in a pipeline — with double quotes grouping arguments and backslashes left alone, so Windows paths
  survive. An offline device keeps a working (dimmed) button, because nothing here can know whether
  a given command needs the peer awake.

  **Open in a terminal window** is what makes `ssh` (or any console program) usable: without it the
  command is detached with no streams, which is right for a graphical program like `mstsc` and
  useless for one that wants a console. With it, Windows gives the child its own console
  (`CREATE_NEW_CONSOLE`, and deliberately *no* stdio redirection — redirect it and the console
  appears but the program is talking to `NUL`), Linux launches the first terminal emulator it finds
  (or `$TERMINAL`), and macOS opens Terminal.app.

  The palette is eight **vivid** colours, and that vivid colour is the only thing chosen: the button
  draws it as a 1px border and derives its interior from it — tinted toward white in light mode,
  sunk toward black in dark mode — with the text colour following the **theme** (black on light,
  white on dark) rather than the hue. Deriving both from one value is what stops yellow being the
  one button that doesn't match, and unit tests hold every fill (rest/hover/pressed, both themes) to
  WCAG AA and every border to a visible contrast against the card behind it. Yellow is a gold rather
  than a lemon for exactly that second reason: a pure vivid yellow border is ~1.5:1 on a white card,
  i.e. not there.

  Actions are stored **per machine**, in the user's own config directory (`actions.json`), and are
  the one piece of per-member state the daemon knows nothing about. Two reasons: the command that
  reaches a device is a property of the machine you're sitting at (`mstsc` on Windows, `xfreerdp` on
  Linux), so the roster would push a Windows command line onto a Mac; and unlike a note or a
  nickname it is an *executable command line*, which has no business in a root-owned store that
  every local user's IPC socket can write and another user's GUI would later spawn. No IPC or
  protocol change.
- **A relay is checked against the relay server before it is saved.** Both the CLI and the GUI
  connect to the relay with the credentials given, from a throwaway endpoint (`relays::probe_relay`,
  exposed as `IpcRequest::ProbeRelay`; IPC protocol → v6). `relay add` asks for the token again if
  the relay won't accept it; the GUI keeps its **Add a relay server** dialog open, reports the
  failure in place, and offers **Try again** or **Add anyway** (a relay that is merely down looks
  exactly like a wrong token from out here). Saving a relay this device can't use is not a small
  mistake: it homes on that relay, and peers lose both the relay path to it and — hole-punching
  being relay-coordinated — usually the direct one. The probe binds its own endpoint, so it cannot
  wedge the live one.
- **Test a relay you've already added**, from the ⇄ button beside it in the relay list. A relay can
  start refusing a token it used to accept (rotated, revoked, redeployed) and nothing else would
  tell you — the symptom is peers quietly losing sight of this device.

## [0.3.3] - 2026-07-13
> **Behaviour change.** `preferred` (the default relay policy) now keeps the public iroh relays
> in the relay map *alongside* your custom relay, instead of replacing them. Devices that were on
> a custom relay become reachable again to peers that lack the relay or its token — that is the
> fix. If you want the old "never touch third-party infrastructure" behaviour, use `only`.

### Fixed
- **A self-hosted relay on some devices but not others silently cut the network in half.** Under
  `preferred`, configuring a custom relay *removed* the public relays from the device's relay map.
  A configured device then advertised the custom relay as its only home relay and had no transport
  that could reach a peer homed on a public one; a peer without the token dialled the custom relay,
  got `401`, and — hole-punching being relay-coordinated — had no direct path either. The two
  groups went mutually invisible while the relay was up and authenticating normally. `preferred`
  was, in effect, identical to `only`. It now keeps both sets in the map, with the path selector
  biasing traffic onto your relay, so partial deployment is no longer fatal.
- **`nullgate-cli relay add` hung indefinitely, and the relay only took effect after a daemon
  restart.** `Endpoint::insert_relay`/`remove_relay` await iroh's bounded socket-actor channel,
  which a peer stuck sending to an unreachable relay can block indefinitely — so the call blocked
  for 20+ minutes on a live mesh, *after* writing `relays.cbor` but *before* swapping the in-memory
  settings. Disk, path selector, endpoint map and reported settings all disagreed, and `relay show`
  truthfully reported the stale value. Settings are now saved, applied and reported atomically and
  return immediately; the endpoint map is updated by a background task with per-call timeouts.
- **The relay watchdog could never have helped, and is gone.** It fired on `home_relay_status()` —
  "can *I* reach my relay" — which was always yes. It could not observe "my peers can't reach my
  relay", which was the actual failure. `NetworkStatus.relay_fallback` went with it.
- **The daemon kept working on requests from clients that had gone away.** A `^C`'d or hung-up
  client left the request running against a socket nobody would read, stranding the task. The
  daemon now cancels an in-flight request when the client disconnects.
- **A non-root daemon crashed instead of falling back to a writable log directory.** `resolve_log_dir`
  accepted any candidate that `create_dir_all` returned `Ok` for — which includes an existing,
  root-owned `/var/log/nullgate` — and then panicked inside `tracing_appender` with
  `PermissionDenied`. It now probes writability before accepting a directory.

### Added
- **Relay settings on Android** (⋮ → *Relay servers*): add a relay with an optional access token,
  remove one, and choose whether the public relays stay as a backup. The phone had no relay surface
  at all before, which is the only reason it stayed reachable through the outage above. Reachable
  before joining a network, since a token-gated relay may be what a device needs in order to join.
- **Relay changes now report whether they were actually applied** (`RelayApply::{Applied, Pending,
  Failed}`), rather than the CLI unconditionally printing "no restart needed". Note that iroh keeps
  a home relay that has left the map until another relay takes over, so switching to `only` while
  your custom relay is unreachable genuinely does need a restart — and now says so.
- Both UIs warn that relay settings are **per-device** and must be set on every member with the
  same URL and token. That warning is the single change most likely to prevent a repeat.

### Changed
- **Installer/`nullgatectl` header art.** The `install.sh` bootstrap and the `nullgatectl`
  manager (Linux + macOS) now print the full-colour Nullgate logo on a colour-capable
  interactive terminal. On pipes, log files, dumb terminals, or when `NO_COLOR` is set they
  fall back to the plain block-glyph wordmark, so redirected output never gets escape codes.
- IPC protocol → **v5**: `GetRelays` answers with a `RelayStatus` (settings + apply state) instead
  of bare `RelaySettings`, and `NetworkStatus` drops `relay_fallback`.

## [0.3.2] - 2026-07-09
> The macOS tarball was built after the `v0.3.2` tag and **includes the sleep/wake fix below**; the
> Windows, Linux, and Android 0.3.2 artifacts predate it and do not. macOS installs (all still on
> 0.3.1, since 0.3.2 originally shipped without a macOS asset) auto-update to it. The other
> platforms are unaffected by the fix — it is macOS-only — and pick up the shared commit in 0.3.3.
### Fixed
- **macOS: a sleeping laptop kept announcing itself to the pool.** Every few minutes on battery,
  macOS takes a *dark wake* — a brief maintenance wake scheduled by Power Nap, unrelated to the
  "wake for network access" setting. The daemon kept running across suspend, so each dark wake
  re-established the mesh for a few seconds and every other device announced "<host> came online",
  all night long. The daemon now watches system power state: it leaves the network before the
  machine sleeps (peers see a clean disconnect instead of an idle timeout) and rejoins only on a
  full wake, ignoring dark wakes. A device the user had manually disconnected stays disconnected.
  Set `NULLGATE_DISABLE_POWER_EVENTS=1` to restore the old behavior.

### Added
- **Custom relay servers.** A device can use its own self-hosted iroh relay(s) instead of the
  public n0 ones, with an optional per-relay access token (sent as `Authorization: Bearer` on the
  relay handshake). Configure from the GUI (**Relay servers** on the main screen, also reachable
  from the "No network yet" page) or the CLI (`nullgate-cli relay add/remove/mode/show/clear`).
  Two policies: **preferred** (default) — run on the custom relays, automatically fall back to the
  public relays while none is reachable and return when one recovers (`engine::relay_watchdog`);
  or **only** — never contact the public relays. A custom iroh `PathSelector`
  (`relays::PreferMyRelaySelector`) additionally ranks paths through the user's own relay above
  any other relay (direct connections still always win). Settings live in `relays.cbor`, apply to
  the **live** endpoint with no daemon restart, and are per-device (every member should configure
  the same relays — a token-protected relay rejects clients without the token). IPC protocol
  bumped to v4 (`GetRelays`/`SetRelays`/`Relays`); `NetworkStatus` gained `relay_fallback`
  (surfaced in Diagnostics and `nullgate-cli status`). New ignored e2e (`relay_e2e`) covers the
  live relay-map swap; `examples/relay_probe.rs` probes a real relay's HTTPS + QUIC endpoints and
  token enforcement.

### Fixed
- **Clicks in the app no longer get "eaten" — the GUI stops rebuilding itself constantly.** The
  main page (and the open Administration/Diagnostics flyouts) used to be torn down and recreated
  whenever anything in the status changed, and status updates arrived several times per second —
  so a click often landed on a button that was destroyed mid-press and did nothing. Three layers
  of fix, end to end:
  - the engine only emits a change event when something **user-visible** changed (a routine
    presence heartbeat used to emit one per member every 3 s, and an idle maintenance tick emitted
    unconditionally; both are now gated, with a ~30 s catch-all so live-read fields like the home
    relay can't go stale);
  - the daemon coalesces bursts of change events into a single status push per subscriber after a
    250 ms quiet window (join-approval events are never delayed), and a lagging subscriber no
    longer silently stops receiving events;
  - the GUI now builds its widget tree **once** and applies every status in place: member rows are
    diffed by node id (title/subtitle/status dot updated on the live widgets, rows added/removed
    only on membership change, reordering moves rows without destroying them), the Diagnostics
    panel updates its rows in place, and the Administration flyout only rebuilds when the data *it*
    shows changes — so Approve/Deny buttons no longer vanish under the cursor. Member rows also
    look up fresh data at click time, so an open detail view can no longer show stale values from
    the moment the row was built.

## [0.3.1] - 2026-07-06
> The macOS tarball was rebuilt and re-uploaded on 2026-07-09 with the tray-icon fix below. The
> version was deliberately not bumped, so existing macOS 0.3.1 installs will not auto-update to it.
### Fixed
- **macOS: a new tray icon appeared every time the GUI was opened.** The tray agent relied on
  GApplication's single-instance guarantee, which GLib implements over a D-Bus session bus — and
  macOS has none. Every `nullgate --agent` therefore became its own "primary" instance, and since
  the GUI spawns the agent on each start, the icons piled up (four live agents were observed after a
  few launches). On macOS the agent now takes an exclusive `flock` on a per-uid lock file and exits
  quietly if another agent already holds it. The macOS installer also stops any stray agents — the
  GUI-spawned ones aren't launchd's to `bootout` — and waits for them to exit before restarting the
  tray, so an upgrade can't leave an agent running from the replaced bundle.
- **macOS: the tray's "Open Nullgate" opened a duplicate window** when one was already up — the same
  missing-D-Bus cause. The GUI now claims a per-uid `flock` plus a small unix socket; a second launch
  pokes that socket so the existing window presents itself, then exits. (`open -a Nullgate.app` looks
  like the natural fix and is not: the agent runs the bundle's `CFBundleExecutable`, so Launch
  Services considers the app already running and just activates the *headless agent* — no window
  ever appears.)
- **macOS: the login tray agent never picked up a changed LaunchAgent plist.** `launchctl bootstrap`
  is a no-op on an already-loaded job, so machines that first loaded the pre-0.2.0 plist kept
  launching `nullgate --minimized` (a GUI window) instead of `--agent`, indefinitely — and once the
  bundle was replaced underneath it, launchd's spawns died with `OS_REASON_CODESIGNING`. `nullgatectl`
  now `bootout`s the job before loading the plist from disk, and verifies the agent actually came up.
- **Intermittent connection drops to remote peers.** Two causes, both in `ipn-core`:
  - The maintenance tick re-dialed *every* member — including unreachable ones — through the
    roster-doc sync (`start_sync`, every 8 s) and presence gossip (`join_peers`, every 3 s). Each
    attempt minted a permanent entry in iroh's unbounded mapped-address cache
    ([iroh#4293](https://github.com/n0-computer/iroh/issues/4293)), so the daemon's memory grew until
    the watchdog restarted it (observed dozens of times a day on one machine), and every restart
    dropped all connections at once. Re-seeding is now throttled to **reachable** members (a live
    mesh connection or a fresh presence heartbeat), with a membership change still re-seeding everyone
    immediately so removals/additions keep propagating within seconds.
  - A **duplicate-connection race**: when both peers dialed each other simultaneously (routine after
    any blip), the second connection's registration orphaned the first, and the orphan's close-watcher
    then unconditionally evicted the *live* connection — a spurious per-peer drop. Both ends now
    apply a deterministic tie-break so they keep the same connection, and the close-watcher only
    evicts the entry that actually closed.
- **A device that just created (or rotated) a network briefly rejected joiners** with "this invite
  code is no longer valid." The creator published its genesis entry and invite to the roster document
  but didn't fold them into its own in-memory roster until the next maintenance tick (up to a few
  seconds), so anyone who connected in that window was checked against an empty roster and turned
  away. The roster is now refreshed immediately after creation/rotation.
### Added
- **Mesh connection lifecycle logging in the daemon log** — connections log when they're
  established, replaced by a duplicate, and closed (with the QUIC close reason: `warn` for an
  unexpected loss, `info` for a deliberate close), so an intermittent drop is diagnosable from the
  default log instead of being silent.
### Changed
- **Refreshed the app + tray icon artwork.** New "gate" mark (a circular badge) replaces the
  previous square icon across every platform, regenerated through the same pipeline: the Windows
  `.ico`, the per-size Linux hicolor PNGs, the macOS `AppIcon.icns`, the Android launcher mipmaps,
  and the Android status-bar silhouette (`ic_stat_nullgate`). Canonical filenames
  (`img/nullgate-icon*`, `img/nullgate-tray-icon*`) are unchanged, so no build wiring changed; the
  previous art moved to `img/old/`.

## [0.3.0] - 2026-07-05
### Added
- **Desktop: the system tray + notifications moved out of the GUI into a lightweight
  background tray agent.** Previously the whole GUI ran hidden at all times to host the tray, so
  closing (or crashing) the GUI made the tray icon vanish even though the network kept running — and
  the daemon, the part that actually matters, had no presence in the tray. Now a small headless
  agent (`nullgate --agent`) autostarts at login, owns the tray icon and all desktop notifications
  ("came online", "wants to join"), and launches the GUI on demand. The GUI is a normal window:
  **closing it just closes it**; the daemon and tray keep running. The tray persists even if the GUI
  crashes.
- **Desktop: the tray now comes up reliably, without ever launching it by hand.** The tray agent
  used to start only from the login autostart entry, so a fresh install (or any session where
  autostart hadn't fired) showed no tray icon until the next login. Now it's (re)launched from every
  angle a session offers — and, being single-instance, a redundant launch is a harmless no-op:
  - **opening the GUI** spawns the agent, so the tray appears the first time you open Nullgate;
  - **installing/upgrading** launches it in your session as the installer's last step — on Linux and
    macOS via `nullgatectl` (a re-login is no longer needed), and on Windows via the auto-updater's
    user-session relaunch;
  - **login** autostart remains the backstop.

  (The agent is independent of the privileged daemon, which — running in an isolated system session —
  can't and doesn't draw UI, so it's deliberately not one of these triggers.)
- **Desktop: a "Restart Nullgate daemon" item in the tray menu.** Bounces the privileged daemon
  (raising your OS's admin prompt) without opening the GUI — on Windows, Linux, and macOS. On
  Windows the restart now **elevates the code-signed daemon directly** (a new `nullgate-daemon
  restart` subcommand via the `runas` verb) instead of shelling out to PowerShell, so there's no
  PowerShell dependency and the UAC prompt shows the *Nullgate* publisher. The GUI's status-banner
  "Restart service" button uses the same path.
- **Desktop: clicking a notification now opens Nullgate.** Peer/join notifications carry a default
  click action (and an "Open Nullgate" button) that brings up the GUI — via the notification action
  on Linux/macOS and the toast activation on Windows.
- **Desktop: a "Start service" / "Restart service" button on the status banner.** When the
  background service is stopped (or reachable but offline / not routing), the app now shows a
  full-width banner across the top with a button that (re)starts the privileged service for you —
  raising your OS's own admin prompt (UAC on Windows, polkit on Linux, the macOS auth dialog)
  instead of making you open a terminal. The banner clears itself once the service reconnects.

### Fixed
- **macOS: Nullgate now shows up in Spotlight (and "Open With") after install.** The installer
  copied `Nullgate.app` into `/Applications` with a plain `sudo cp -R`, which never triggered the
  Launch Services registration + Spotlight import that a Finder drag does — so the app ran but was
  invisible to Spotlight search. `nullgatectl` now registers the bundle with `lsregister` and runs
  `mdimport` after every install **and update** (a bundle replacement can stale out the existing
  Launch Services record), and copies the bundle with `ditto` instead of `cp -R` to preserve its
  signature/xattr metadata cleanly.
- **Keyboard navigation no longer jumps back to "Administration" every few seconds** (most visible
  on Windows). A periodic status push would rebuild the main page's widget tree, dropping keyboard
  focus so GTK reset it to the first row. This had been "fixed" several times by removing whichever
  volatile field was churning the render signature, only to regress when a new one crept in. The
  durable fix decouples keyboard correctness from the signature: `render_all` now saves the focused
  row and restores it after any rebuild, so focus survives regardless of what triggers one. Also
  dropped the volatile `observed_addr` (an ip:port whose UDP port flaps as iroh re-probes paths) from
  the render signature so the page stops needlessly rebuilding every few seconds in the first place.
- **Windows: the tray GUI now restarts after an auto-update.** The daily `NullgateUpdate` task
  runs as SYSTEM (session 0) while the GUI runs in the logged-in user's interactive session, so
  the MSI's Restart Manager could close the GUI but never relaunch it across that boundary —
  leaving the daemon updated but the tray app gone (or stale) until the next login. The updater
  (`packaging/windows/nullgate-update.ps1`) now closes the GUI before applying the MSI (so
  `nullgate.exe` swaps in place with no pending reboot) and relaunches it minimized in the user's
  session via a one-shot Interactive scheduled task. Linux/macOS already self-relaunch on the
  daemon's version change (`ipn-gui` `restart_self`), where the updater shares the user session;
  this closes the Windows-only gap.

### Changed
- **Desktop: login autostart now launches the tray agent, not the full GUI.** The per-user
  autostart entry (Windows Run key, macOS LaunchAgent, Linux XDG autostart) runs `nullgate --agent`
  — a fraction of the footprint of the old always-hidden GUI. The tray's **"Quit Nullgate"**
  disconnects from the network and quits the agent; the GUI window closes independently.
- **Desktop: status and join-request alerts now use full-width `adw::Banner`s.** The
  "service not running" state moved from a full-page notice to a proper banner (with the new Start
  button) plus a compact illustration, and the offline / routing-off banners now span the whole
  window instead of being clamped to the content column. The **flashing red "Join Request" chip**
  on the Administration row is replaced by a banner — "A new device has requested network access"
  (pluralized for several) — with a **Review** button that opens the emoji-SAS approval screen.
- **Android: the app now hides admin actions that Peers and Controllers can't use**, matching the
  desktop apps instead of showing every option to everyone and letting the engine reject the
  unauthorized taps. The overflow menu, join-request approvals, and member actions are now gated by
  the viewer's role: Peers see only Activity log, their own member details, and Leave network;
  Controllers additionally get the Peer join ticket, Settings (disable-remote-access / hide),
  Rename network, and can remove Peers; the originator keeps the full set (Controller ticket,
  freeze, rotate, delete, originator-key, promote/demote, remove anyone). Pending join-request
  cards no longer render for Peers, and a Controller can no longer be offered "Remove" on another
  Controller. Engine-side authorization is unchanged; this only aligns the Android UI's affordances
  with the roster rules the desktop GUI already follows.
- **Android: added "Restore originator access"** for members who hold the master recovery key but
  aren't the originator on this device (Peers and Controllers), matching the desktop's
  backup-vs-restore split. The originator's "Originator key" dialog still exports and imports; the
  new entry is import-only. This closes the last Android/desktop role-UI gap for the release.

## [0.2.5] - 2026-07-05
### Fixed
- **Cmd+Q now hides Nullgate to the tray on macOS** (previously it did nothing — the quit
  accelerator was bound only to `<Ctrl>q`, and Cmd is the Meta modifier on macOS). Cmd+Q now
  matches the window's close button / Alt+F4 elsewhere: it hides the window and leaves the app
  running in the tray. Fully quitting stays reserved for the tray's "Quit Nullgate" item.
- **macOS routing now actually turns on.** The TUN interface was always created with the fixed
  name `nullgate`, which macOS's `utun` kernel control rejects (utun devices must be named
  `utunN`), so `RealTun::open` failed on macOS and the app permanently showed the "Routing off —
  start the daemon elevated to carry traffic" banner even with an assigned `10.99.0.x` address.
  The interface is now only explicitly named on Windows/Linux; on macOS the OS assigns the utun
  index.

### Changed
- **Release/distribution repo renamed `iroh-private-network` → `nullgate`.** Updated the
  in-product updaters (`packaging/{windows,linux,macos}` update scripts), `install.sh`, the systemd
  unit `Documentation=` URLs, `INSTALL.txt` files, the workspace `repository` field, the GUI
  About-window website/issue URLs, and the docs/README release links to
  `github.com/steeb-k/nullgate`. The `NULLGATE_BINARIES_REPO` override still lets you point the
  updaters elsewhere.

## [0.2.4] - 2026-07-04
### Changed
- **New app + tray icons across every platform.** The redrawn "gate" mark replaces the old
  `icon-stacked`/`tray-icon-splash` art. The main app icon now ships as `img/nullgate-icon.*`
  (a fresh multi-resolution `img/nullgate-icon.ico` embedded in the Windows `.exe`, the per-size
  hicolor PNGs for Linux, and the macOS `AppIcon.icns` — now with a native 1024 slot instead of an
  upscale). The tray/status icon is `img/nullgate-tray-icon-64.png` on Windows/macOS/Linux. On
  Android the launcher icon becomes the new main icon (bitmap mipmaps, replacing the old adaptive
  vector) and the foreground-service notification uses a white-silhouette status icon
  (`ic_stat_nullgate`) derived from the tray mark, replacing the stock lock glyph.

## [0.2.3] - 2026-07-03
### Added
- **Memory watchdog (iroh #4293 stopgap).** The daemon now samples its own resident memory every
  30 s and, past a limit (default 1024 MB), logs why and restarts itself via the service manager.
  This bounds the unbounded growth of iroh 1.0's per-remote mapped-address cache
  (`socket::mapped_addrs::AddrMap`, [iroh#4293](https://github.com/n0-computer/iroh/issues/4293)) —
  which the captured minidump showed reaching a single ~80 GB allocation before the `0xc0000409`
  OOM abort. The cache lives inside the iroh node (built once per process, never rebuilt), so only a
  restart reclaims it. Overrides: `NULLGATE_MEM_LIMIT_MB` (`0` disables), `NULLGATE_MEM_CHECK_SECS`.
  Remove once the upstream eviction fix ships. `ipn-daemon/src/watchdog.rs`; see
  `docs/architecture.md`.
- **"Came online" notification debounce.** To absorb the brief presence blips from those
  watchdog restarts, the desktop app only announces a peer as back online once it has been offline
  for at least 2 minutes (`NULLGATE_ONLINE_DEBOUNCE_SECS`), so a restarting device no longer spams
  every machine on the mesh. Pure decision logic factored out of `notify_newly_online` with unit
  tests.

## [0.2.2] - 2026-07-02
### Added
- **Capture non-panic crashes (Windows).** The daemon's crash logging previously only caught Rust
  `panic!`s, but the recurring service crash is a `0xc0000409` fastfail from `abort()` (stack
  overflow / allocation failure / native abort), which bypasses the panic hook. Under a service the
  daemon now also (1) points the raw stderr handle at the crash log so the Rust runtime's own fatal
  messages (`… has overflowed its stack`, `memory allocation of N bytes failed`, `fatal runtime
  error: …`) are captured instead of discarded, and (2) installs a vectored exception handler that
  logs the code + faulting address of first-chance hardware faults (access violation, stack
  overflow) before they abort. The MSI also registers **WER LocalDumps** for `nullgate-daemon.exe`
  (minidump to `%ProgramData%\Nullgate\logs\dumps`) so even a bare fastfail leaves an analyzable
  dump. New `NULLGATE_CRASH_SELFTEST=av|stackoverflow|abort` (and `NULLGATE_FORCE_NO_CONSOLE=1`)
  exercise these paths. See `docs/building.md`.

### Fixed
- **Daemon memory leak on an unreachable member.** The periodic maintenance tick fanned out a fresh
  `endpoint.connect()` to every roster member we weren't currently connected to — every 3 seconds,
  with no timeout and no dedup. A member that was offline (or on a thrashing network path) never
  entered the connected set, so each tick spawned another dial whose iroh connection/path state was
  never reclaimed; over a multi-day run the daemon's committed heap grew into the tens of GB (and the
  resulting paging pinned the CPU). Dials are now de-duplicated (one in-flight attempt per peer) and
  each is bounded by a 20 s timeout, with the in-flight slot freed on every exit path so retries
  still happen. New `engine::spawn_dials` with `#[tokio::test]` coverage of the invariant (repeated
  ticks never stack dials; the slot frees on timeout). See `docs/architecture.md`.

## [0.2.1] - 2026-07-01
### Added
- **Headless verification over the CLI.** `nullgate-cli` can now drive a full join without a GUI.
  `join <ticket>` prints the join **verification code as words** (e.g. `Rocket`, `Anchor`, …)
  instead of emojis — a terminal can't reliably show or compare the glyphs — and blocks until a
  member approves. A new `watch` command streams incoming join requests with the same words plus
  the exact `approve`/`deny` commands to run. The words are derived from the identical SAS the GUI
  turns into emojis, so a GUI ↔ CLI join still compares correctly. New `ipn_ipc::sas_words` /
  `ipn_core::admission::{SAS_WORD, word_for_emoji, sas_words}`. See the README "Headless / CLI"
  section and `docs/security.md`.
- **Daemon crash logging + service auto-recovery (all platforms).** The privileged daemon now
  writes its own on-disk log and installs a panic hook that records the panic message, source
  `file:line`, and a backtrace **synchronously** to a crash log before the process exits — so a
  crash's cause is captured even under a service manager (previously `tracing` went to a discarded
  stdout, leaving Windows service crashes — a Rust panic surfacing as a `0xc0000409` fastfail —
  invisible). Logs live in `%ProgramData%\Nullgate\logs` (Windows), `/var/log/nullgate` (Linux),
  and `/Library/Logs/Nullgate` (macOS); override with `NULLGATE_LOG_DIR`. Windows service install
  (both `nullgate-daemon install` and the MSI, via `util:ServiceConfig`) now configures SCM
  failure actions (restart after 5s/15s/60s, reset the counter after a day) to match Linux
  (`Restart=on-failure`) and macOS (`KeepAlive`); the Linux unit also disables the systemd
  start-rate limit so a crash-loop keeps recovering. New hidden `nullgate-daemon recover` command
  applies recovery to an already-installed Windows service without a reinstall, and
  `NULLGATE_PANIC_SELFTEST=1` forces a panic to validate the crash-log + restart pipeline. See
  `docs/building.md`.
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

### Changed
- **Linux installer no longer implies GTK is mandatory.** `nullgatectl --install` treated missing
  GTK libraries as a blocking "install … then re-run" warning, but GTK is needed only by the
  desktop GUI — the daemon and `nullgate-cli` are headless and don't link it. The message is now a
  note that the install completes and works headlessly, and `INSTALL.txt` / the README / the Linux
  packaging doc say the same.
- **Install-script wordmark banner.** The `install.sh` one-liner and both `nullgatectl` managers
  (Linux + macOS) now print the Nullgate wordmark at the top of their output.

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
