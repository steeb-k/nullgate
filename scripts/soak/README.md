# Android soak-test harness

Long-running, unattended reliability + data-usage tests for the Android app,
run against the emulator. Built to reproduce the two production failure modes:

- **the silent death**: after doze, connectivity dies with *no network change
  visible to Android*, and the app never recovers without a manual VPN toggle;
- **the data burn**: tens of GB/month while mostly disconnected (a broken iroh
  endpoint probes at its *maximum* rate, so being dead is the expensive state).

The harness needs **zero app changes**: reachability of the phone is judged
from the desktop side (an isolated throwaway daemon + `nullgate-cli status`,
where "online" = a live QUIC connection — ground truth), and data usage comes
from per-UID `dumpsys netstats` on the emulator. Raw dumps are saved alongside
every parsed number so a week-long run can be re-analyzed without re-running.

## Isolation

Nothing here touches a real Nullgate install. The soak daemon runs with its own
`--socket` and `NULLGATE_DATA_DIR` under `target\soak\`, TUN disabled, secrets
file-backed, on a throwaway network named `soaknet` whose only members are the
soak daemon (originator) and the emulator.

## One-time setup

Prereqs: the Android dev environment from `docs/android-packaging.md` (SDK,
NDK, cargo-ndk, the `seed_api35` AVD), plus a Windows host that will not sleep
mid-run (Settings > Power > never sleep while plugged in).

```powershell
pwsh -File scripts\soak\setup-soak.ps1          # add -SkipAppBuild to reuse the APK
```

This builds the desktop binaries, starts the soak daemon, creates `soaknet`,
boots the emulator with the app, and walks you through the one interactive
part: joining from the emulator UI (the script types the ticket over adb and
auto-approves the join) and granting the VPN consent dialog. Re-runnable; it
skips whatever already exists.

## Running scenarios

```powershell
pwsh -File scripts\soak\run-soak.ps1 -Scenario <name> [knobs]
```

| Scenario | Report test | What it proves | Typical invocation |
|---|---|---|---|
| `idle` | T1 | An idle, dozing phone stays reachable and cheap | `-Scenario idle -Hours 24` |
| `doze` | T2 | Recovery after every doze/wake cycle | `-Scenario doze -Cycles 24 -HoldMinutes 45` |
| `flap` | T3 | Survives wifi↔cell↔airplane transitions | `-Scenario flap -Hours 12` |
| `blackhole` | **T4** | **The killer test** — unassisted recovery from a *silent* connectivity death | `-Scenario blackhole -Cycles 12 -HoldMinutes 20 -WithDoze` |
| `kill` | T6 | The system restarts the VpnService after a crash | `-Scenario kill -Cycles 10` |
| `attribution` | T7 | Bytes land in the right netstats bucket (wifi billed as mobile = bug) | `-Scenario attribution -Hours 4` |
| `leak` | T8 | No RSS / per-dial cost growth with an unreachable member (iroh#4293) | `-Scenario leak -Hours 48` |

`blackhole` works by installing root iptables DROP rules **inside the guest**
(`adb root`, AOSP images only) for outbound UDP + TCP 80/443/8443, so Android
still sees a perfectly healthy network — the exact shape of the production
bug. Rules are removed in a `finally`; after a hard kill, `Disable-Blackhole`
in `soaklib.ps1` (or an emulator reboot) cleans up. Do NOT "simplify" this
back to host-firewall rules against the qemu process: blocking qemu's host
sockets wedges its slirp NAT **permanently** — the guest transmits nothing
even after the rules are removed, until the emulator restarts (both early
blackhole runs measured exactly that artifact: tx=0B all night).

Knobs: `-RecoveryTimeoutSec` (default 600; a cycle that hasn't recovered by
then is a failure — tighten to 60 once the fixes land), `-MaxIdleMbPerHour`
(0 = record-only baseline; set a budget to make `idle`/`leak` assert on data).

## Results

Each run writes `target\soak\runs\<stamp>-<scenario>\`:

- `events.jsonl` — every cycle, sample, and recovery measurement, timestamped;
- `netstats-*.txt` — raw per-UID dumps at every sample point;
- `logcat.txt` — full `*:I` capture for the run;
- `summary.json` — pass/fail, cycle + failure counts, recovery p50/p95/max,
  total MB and MB/hour.

Exit code 0/1 mirrors `summary.json`'s verdict so runs chain from a wrapper.

## Suggested week (baseline first)

Run the whole set against the **current, known-broken build first** and keep
those numbers: they are the before-picture every fix is judged against, and
`blackhole` failing today *confirms* the harness reproduces the field bug.

1. `attribution -Hours 4` — settles the mis-attribution question immediately
2. `blackhole -Cycles 12 -HoldMinutes 20 -WithDoze` — overnight
3. `idle -Hours 24` — the data-burn baseline
4. `doze -Cycles 24 -HoldMinutes 45` — overnight
5. `flap -Hours 12`
6. `kill -Cycles 10`
7. `leak -Hours 48` — weekend

Don't call the Android client fixed until `blackhole`, `doze`, `flap`, and
`idle` pass clean with `-RecoveryTimeoutSec 60` for a full week of nightly
runs, and `idle` holds `-MaxIdleMbPerHour` at the agreed budget.

## Fidelity caveats (what the emulator can and can't prove)

- **Emulator doze is not a phone's CPU suspend.** `force-idle deep` applies
  Android's doze *policies* (network restrictions, alarm deferral) but the
  guest vCPU keeps running, so timers don't freeze the way they do on a Pixel.
  That's why `blackhole -WithDoze` exists: firewall + doze together is the
  closest reproduction of the real "woke up dead" state. A green `doze` run
  alone is necessary, not sufficient.
- **The emulator's cellular is emulated** — same host NAT as wifi, and on the
  `seed_api35` image everything rides one `eth0` ident typed as cellular:
  netstats never shows a distinct WIFI bucket, so `attribution` can only bound
  gross mis-bucketing (e.g. bytes landing in the VPN ident) on this AVD. The
  definitive wifi-vs-mobile attribution check is the real phone over adb
  (`dumpsys netstats detail`, uid from `dumpsys package`).
- **GrapheneOS ≈ AOSP** for doze/App-Standby semantics, so results transfer to
  the actual phone unusually well — but the final sign-off for the field bug
  is a day of the real Pixel on the fixed build.
