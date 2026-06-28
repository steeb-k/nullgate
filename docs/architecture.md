# How it works

IPN is a peer-to-peer virtual LAN built on [iroh](https://www.iroh.computer). There are no
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
The roster is an append-only set of **signed entries** (`Add` / `Remove` / `Freeze`) stored in
an [iroh-docs](https://github.com/n0-computer/iroh-docs) document — a replicated multi-writer
store that every member syncs. Each node folds the entries into the current membership by
applying role rules. See [security.md](security.md) for the full trust model.

Presence (who's online, hostname, friendly label, last-seen) is broadcast separately over
[iroh-gossip](https://github.com/n0-computer/iroh-gossip) on the private rendezvous topic, each
heartbeat **signed** by the device. The **hostname** is the device's *actual current* OS hostname
(re-read on every heartbeat — it tracks the real machine name and isn't user-editable); the
**label** is an optional friendly name the member sets for itself. The public IP shown for a peer
is the address your node actually observes for it (so a peer can't spoof its own).

## Components (crates)
- `ipn-core` — the engine: iroh node, signed roster, admission + emoji verification, presence,
  and TUN routing. UI- and IPC-agnostic (also the basis for a future Android build).
- `ipn-ipc` — the contract + transport between the GUI and the daemon (a length-prefixed CBOR
  protocol over a named pipe on Windows / a Unix socket on Linux/macOS).
- `ipn-daemon` — the **privileged** part: owns the iroh node + TUN and serves the GUI over IPC.
  Runs as a LocalSystem service on Windows, or with `CAP_NET_ADMIN` (via `setcap`) / systemd on
  Linux.
- `ipn-gui` — the GTK4 + libadwaita desktop app (binary `ipn`). **Unprivileged** — it only
  talks to the daemon, so day-to-day use never needs admin/root.
- `ipn-cli` — a small headless client (status / create / join / approve / remove / rotate …),
  handy for scripting and testing.

### Why the daemon/GUI split
Creating the virtual network interface needs elevated privilege; a GUI does not. Splitting them
means the privileged work is isolated in a tiny background service while the app you click runs
as you — so you elevate once at install time, never per launch.
