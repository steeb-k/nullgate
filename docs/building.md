# Building from source

A Rust workspace. Desktop builds need GTK4 + libadwaita available to `pkg-config`.

## Prerequisites
- Rust (stable, 1.85+).
- **Linux:** `sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential`
- **Windows:** the MSVC toolchain and GTK4 + libadwaita via
  [gvsbuild](https://github.com/wingtk/gvsbuild) (the scripts assume it's at `C:\gtk`).
- **macOS:** GTK4 + libadwaita via Homebrew (`brew install gtk4 libadwaita pkg-config`).

## Run in development
The daemon (privileged, owns the TUN) and the GUI (unprivileged) run as separate processes.

```sh
# Terminal 1 — the daemon:
#   Linux (once): sudo setcap cap_net_admin,cap_net_raw+ep target/debug/ipn-daemon
#   Windows:      run from an elevated shell
cargo run -p ipn-daemon

# Terminal 2 — the GUI:
cargo run -p ipn-gui
```

Without a running daemon the GUI shows a "daemon not running" page. If the daemon lacks routing
privilege, membership + presence still work and the GUI shows a "routing off" banner.

The headless client is handy for testing without the GUI:
```sh
cargo run -p ipn-cli -- status
cargo run -p ipn-cli -- create home
```

## Tests
```sh
cargo test -p ipn-core                 # unit tests
# end-to-end tests open real iroh endpoints, so they're #[ignore]d by default:
cargo test -p ipn-core --test engine_e2e   -- --ignored   # create / join / verify / connect
cargo test -p ipn-core --test delete_e2e   -- --ignored   # delete boots everyone, no ghosts
cargo test -p ipn-core --test rotate_e2e   -- --ignored   # rotate locks out old-ticket devices
```

## Daemon logs & service recovery
The privileged daemon writes its own rotating log plus a crash log, independent of the console
(which a service manager discards). A panic hook records the panic message, source `file:line`,
and a backtrace **synchronously** to `nullgate-daemon-crash.log` before the process can abort.

Not every crash is a Rust `panic!`, though. A `0xc0000409` fastfail comes from `abort()` — a
**stack overflow**, an **allocation failure**, or a native `abort()` — and deliberately bypasses
the panic hook (and SEH, vectored handlers, and the unhandled-exception filter). Three additional
nets cover those on Windows (see `crates\ipn-daemon\src\logging.rs`, active only under a service,
i.e. no console):
- **Reclaimed stderr** — `STD_ERROR_HANDLE` is pointed at the crash log so the Rust runtime's own
  fatal messages (`thread '…' has overflowed its stack`, `memory allocation of N bytes failed`,
  `fatal runtime error: …`) are captured instead of discarded.
- **Vectored exception handler** — logs the code + faulting address of first-chance hardware faults
  (access violation `0xc0000005`, stack overflow `0xc00000fd`) before they convert to an abort.
- **WER LocalDumps** — the MSI registers a per-process minidump under
  `HKLM\…\Windows Error Reporting\LocalDumps\nullgate-daemon.exe`, so even a bare fastfail (which
  the in-process nets can't see) leaves a `.dmp` in `%ProgramData%\Nullgate\logs\dumps` to open with
  WinDbg/`cdb` against the matching `nullgate_daemon.pdb`. Enable it on a machine that predates the
  MSI change with an **elevated** PowerShell:
  ```powershell
  $k='HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\nullgate-daemon.exe'
  New-Item $k -Force | Out-Null
  New-ItemProperty $k DumpFolder 'C:\ProgramData\Nullgate\logs\dumps' -PropertyType ExpandString -Force | Out-Null
  New-ItemProperty $k DumpType 2 -PropertyType DWord -Force | Out-Null   # 2=full, 1=mini
  New-ItemProperty $k DumpCount 10 -PropertyType DWord -Force | Out-Null
  ```

Log directory (override with `NULLGATE_LOG_DIR`; falls back to `<data-dir>/logs` when the
privileged path isn't writable, e.g. an unprivileged foreground run):

| Platform | Directory | Rotating log | Crash log |
|----------|-----------|--------------|-----------|
| Windows  | `%ProgramData%\Nullgate\logs` | `nullgate-daemon.log.<date>` | `nullgate-daemon-crash.log` |
| Linux    | `/var/log/nullgate`           | ″ (also in `journalctl -u nullgate-daemon`) | ″ |
| macOS    | `/Library/Logs/Nullgate`      | ″ (launchd also writes `nullgate-daemon.log`) | ″ |

**Auto-restart.** Each platform restarts the daemon if it exits unexpectedly:
- **Windows** — SCM failure actions (restart after 5s, 15s, then 60s; failure counter resets after
  a day) set at install time by both `nullgate-daemon install` and the MSI (`util:ServiceConfig`).
  To repair an older install that predates this, run **elevated**: `nullgate-daemon recover`
  (or `sc.exe failure NullgateDaemon reset= 86400 actions= restart/5000/restart/15000/restart/60000`
  then `sc.exe failureflag NullgateDaemon 1`).
- **Linux** — `Restart=on-failure` with the systemd start-rate limit disabled so a crash-loop keeps
  recovering.
- **macOS** — launchd `KeepAlive`.

To verify the crash → crash-log → restart pipeline on a real install, set one of these for the
daemon (then unset): `NULLGATE_PANIC_SELFTEST=1` (Rust panic → panic hook), or
`NULLGATE_CRASH_SELFTEST=av|stackoverflow|abort` to exercise the non-panic nets — `av` (access
violation → VEH), `stackoverflow` (→ VEH + reclaimed stderr), `abort` (bare fastfail → only the WER
dump). `NULLGATE_FORCE_NO_CONSOLE=1` forces the service-mode capture path from a terminal.

## Android
The Android app (`android/`, Kotlin/Compose over the `ipn-mobile` UniFFI facade) builds with the
Android SDK + NDK. One-time setup: JDK 17, Android SDK 35, NDK r27c, `cargo install cargo-ndk`, and
`rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android`.

```sh
cargo ndk -t arm64-v8a build -p ipn-core      # quick cross-compile check of the engine
pwsh -File scripts/run-android.ps1            # build APK + install + launch on the emulator
cd android && ./gradlew :app:assembleDebug    # or build the APK directly
```
Full detail (toolchain, ABIs, signing, the `VpnService` routing model) is in
**[android-packaging.md](android-packaging.md)**.

## Packaging & releasing
Building the installers and cutting a release has its own guides:
**[releasing.md](releasing.md)** (the end-to-end flow) plus the per-platform detail in
**[windows-packaging.md](windows-packaging.md)** (signed MSI + auto-updater),
**[linux-packaging.md](linux-packaging.md)**, **[macos-packaging.md](macos-packaging.md)**, and
**[android-packaging.md](android-packaging.md)**.
