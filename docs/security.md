# Security model

This describes how Nullgate decides who is in a network and how access is taken away. It assumes the
networking background in [architecture.md](architecture.md).

## Identities
- A **device** is identified by its NodeId (an ed25519 public key). iroh authenticates this at
  the transport layer, so the peer on the other end of a connection is provably that key.
- A **network** is identified by its **secret** (in the join ticket). From it are derived the
  discovery rendezvous, the admission PSK, and the roster's document namespace.
- The **originator** holds a separate, exportable **master key** — the authority for removing
  members, freezing the roster, and rotating the secret. It can be **backed up** as an
  `ipnkey1…` recovery code and **re-imported** on another member of the same network to restore
  admin powers after device loss (a code for a different network is refused). Treat the code like
  a master password — anyone who has it can administer the network.

## Admission (joining)
1. A joiner connects to an existing member and proves it holds the network PSK (an HMAC
   challenge bound to both NodeIds and fresh nonces — knowing the rendezvous alone isn't enough).
2. Both sides derive an identical **emoji short-authentication-string (SAS)** from the session.
   The two humans compare it. This catches a wrong/MITM'd identity and is a friendly stand-in
   for eyeballing a 64-character key.
3. An existing member approves, which writes a signed `Add` for the joiner (web of trust). The
   originator's device is the genesis member, bootstrapped by the master key.

## The roster and "role rules"
Membership is a set of **signed entries** in a replicated document:
- `Add` counts only if signed by a **current member** (or the originator).
- `Remove` / `Freeze` count only if signed by the **originator master key**.

Why it's done this way: the document's write capability is the network secret, which **every
member holds and which can't be un-shared** — you can't claw a secret back. So security doesn't
come from gatekeeping *who can write*; it comes from gatekeeping *which writes count*. A removed
device can still scribble entries into the shared document, but its signature is no longer that
of a current member, so every node — including its own — ignores them. (The
`removed_member_cannot_forge` test proves this even over real replication.)

## Secrets at rest
The device key, the network secret, and the originator master key are stored in the OS keystore
(Windows Credential Manager, macOS Keychain, Linux Secret Service) via the `keyring` crate. On a
headless host with no keystore they fall back to `0600` files under `<data_dir>/secrets/`. A
per-secret marker records that a secret is keystore-backed, so if the keystore is temporarily
unavailable the daemon errors clearly rather than regenerating identity (which would silently
evict the device). `network.cbor` holds only non-secret fields (name, subnet, originator pubkey).
This assumes one daemon instance per machine/user account.

## Geolocation
Member "Location" (City, Country) is resolved **only by the originator**, which downloads the
DB-IP City database (CC BY 4.0) and looks up each member's advertised public IP **locally** — no
per-IP queries to any third party. It then propagates the resolved strings to members in a gossip
message **signed by the originator master key**, so members trust them without needing the
database or any internet lookup of their own. Attribution ("IP Geolocation by DB-IP",
https://db-ip.com) is shown wherever Location appears, as the CC BY 4.0 license requires.

## Shared-doc abuse
Every member holds the iroh-docs write capability (that's what enables web-of-trust adds), so a
malicious member could append junk to the replica. Reading is bounded — only `e/` entries, small
values, and a capped count are folded — so spam can't OOM or peg a member. Bounding the *on-disk*
growth a member can cause is not fully solved (it needs originator-signed snapshots + pruning, a
deferred change); the practical responses are to **remove** the offender or **rotate** the secret
(which abandons the spammed namespace for a fresh one).

## Roster ordering and timestamps
Entries carry a member-chosen timestamp used only to order a concurrent set, with the content
hash as a tiebreak. Timestamps are a hint, not a trust anchor, so the fold hardens against
manipulation: entries dated implausibly far in the future are dropped, and a member cannot sign
an `Add` backdated to before its own admission. One residual remains — a current (trusted) member
could backdate an `Add` into a past *unfrozen* window to slip a device past a freeze; fully
closing that needs causal ordering (a hash-linked DAG / version vectors) and is deferred. The
backstop is that the attacker is already a trusted member and the originator can remove the
device or rotate the secret.

## Taking access away
- **Remove a member** — the originator signs a `Remove`. It propagates to connected peers; each
  node rebuilds the roster, drops the device from routing, and tears down any live connection to
  it (so no "ghost" connection survives). A device that finds itself removed **self-evicts**:
  it drops its connections and clears the now-dead network.
- **Freeze** — the originator stops any new joins until unfrozen.
- **Rotate** (the hard cutoff) — the originator boots everyone and restarts the network under a
  brand-new secret, then shares a new ticket with the devices to keep. Because the secret drives
  discovery, admission, and the roster namespace, anyone holding the old ticket is locked out
  entirely — including a device that happened to be **offline** when it was removed (the one
  case a single `Remove` can't reach until that device reconnects).

## What this does and doesn't protect
- Traffic between members is end-to-end encrypted by iroh; non-members can't read it, and the
  network's discovery rendezvous is private so outsiders can't even find it.
- Trust is **device-granular**: access is per device key. Losing a device means removing (or
  rotating out) its key.
- This is a personal-scale trust model (a handful of your own devices), not a hardened
  multi-tenant system. Notably, members are mutually trusting once admitted; web-of-trust means
  any member can vouch in another (the originator is the backstop via remove/rotate).
