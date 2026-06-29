# Development & contributing

How to add a feature to Nullgate and document it. (Agents: `CLAUDE.md` at the repo root is the
authoritative version of this; keep the two in sync.)

## The shape of a feature
Most features cross the same layers, in order. Do each step, then move outward:

1. **Engine (`ipn-core`).** Implement the behavior as a method on `Engine` (or a new module).
   State lives behind the async `Mutex`; do network I/O off-lock. Emit `EngineEvent::Changed`
   (or a specific event) when something the UI shows changes.
2. **Tests.** Unit tests in `ipn-core`. If the feature touches membership, connectivity, or
   revocation, add an **ignored e2e smoke test** in `crates/ipn-core/tests/` proving the real
   property on live nodes (templates: `engine_e2e`, `delete_e2e`, `rotate_e2e`). Set
   `NULLGATE_DISABLE_TUN=1` in tests.
3. **IPC (`ipn-ipc`).** Add an `IpcRequest` variant (and `IpcResponse`/`IpcEvent` if needed).
4. **Daemon (`ipn-daemon`).** Handle the request in `handle_request` / map the event in
   `map_event`.
5. **CLI (`ipn-cli`).** Add a subcommand — the fastest way to test the IPC path headlessly.
6. **GUI (`ipn-gui`).** Add a control + `UiMsg` path; never block the GTK thread.
7. **Docs + changelog** (see below).

## Definition of done
- Compiles on Windows **and** Linux.
- `cargo test -p ipn-core` passes; relevant e2e smoke test passes.
- Docs updated and a `CHANGELOG.md` entry added.

## Documentation conventions
Update docs **in the same change** as the code:

| What changed | Update |
|--------------|--------|
| User-visible behavior | `README.md` — plain language, no internals |
| Components / data flow | `docs/architecture.md` |
| Trust / identity / revocation | `docs/security.md` |
| Build, test, packaging, release | `docs/building.md` / `docs/releasing.md` |
| Anything | a `## [Unreleased]` bullet in `CHANGELOG.md` |
| Crates, commands, or this workflow | `CLAUDE.md` (and this file) |

Rules of thumb: README is for a mildly-technical user and stays jargon-free; `docs/` is precise
and may go deep; when you move detail out of the README, link to the doc that now holds it.

## Testing
```sh
cargo test -p ipn-core                                  # unit (fast)
cargo test -p ipn-core --test <name> -- --ignored       # e2e (real iroh endpoints)
```
See `docs/building.md` for the full list and toolchain setup.

## Style
- `anyhow` for errors in engine/daemon/cli; `io::Result` in transport; GUI → toast on failure.
- Comments explain *why*. Match the surrounding file's voice.
- Keep `ipn-ipc` light (no heavy deps beyond what the protocol/transport need).
- iroh ecosystem crates are pinned together — bump together, then `cargo tree -d`.
