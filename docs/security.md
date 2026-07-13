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
  `ngkey1…` recovery code and **re-imported** on another member of the same network to restore
  admin powers after device loss (a code for a different network is refused). Treat the code like
  a master password — anyone who has it can administer the network.

## Admission (joining)
1. A joiner connects to an existing member and proves it holds the network PSK (an HMAC
   challenge bound to both NodeIds and fresh nonces — knowing the rendezvous alone isn't enough).
2. Both sides derive an identical **emoji short-authentication-string (SAS)** from the session.
   The two humans compare it. This catches a wrong/MITM'd identity and is a friendly stand-in
   for eyeballing a 64-character key. The GUI shows the emojis; text-only clients (`nullgate-cli`,
   over SSH) render the same code as the equivalent **words** — a terminal can't reliably display
   or compare the glyphs — so the comparison holds across a GUI ↔ CLI join.
3. An existing member approves, which writes a signed `Add` for the joiner (web of trust). The
   originator's device is the genesis member, bootstrapped by the master key.

## The roster and "role rules"
Membership is a set of **signed entries** (`ipn-roster-v2`) in a replicated document. Every member
has a **role** — `Peer` or `Controller` — baked into its `Add`. The **originator** (master-key
holder) is a separate, higher authority and isn't a roster role. The fold rules:
- `Add` counts only if signed by the **originator** (any role, no invite needed) **or** by a
  current **Controller** citing the *current* invite nonce for the role being granted (and, if the
  invite is single-use, an unconsumed nonce). Peers can't admit anyone.
- `Remove` counts if signed by the **originator** (any target) **or** by a current **Controller**
  whose target is a **Peer** (Controllers can't evict Controllers or the originator).
- `SetRole` (promote/demote in place) and `Freeze` are **originator-only**.
- `SetInvite` sets the current join nonce for a tier. A **Peer** invite may be set by the
  originator or any Controller; a **Controller** invite is **originator-only and always
  single-use**. Latest-by-(ts, id) wins, so regenerating an invite (or toggling Peer single-use)
  mints a new code that invalidates the prior one **for new joins** — without rotating the secret,
  so no current member loses access. A Peer ticket reads the current nonce (stable to re-show); a
  Controller ticket mints a fresh single-use nonce each time.

Why it's done this way: the document's write capability is the network secret, which **every
member holds and which can't be un-shared** — you can't claw a secret back. So security doesn't
come from gatekeeping *who can write*; it comes from gatekeeping *which writes count*. A removed
device (or a Peer) can still scribble entries into the shared document, but the fold rejects them,
so every node — including its own — ignores them. (The `removed_member_cannot_forge` test proves
this even over real replication.)

Residual to note: a single-use **Controller** invite nonce rides in the ticket, so a *malicious
current Controller* who sees it could race to consume it for a colluding device before the
intended joiner's `Add` folds. The blast radius is one Controller per originator-issued invite
(single-use), and it requires an already-trusted-Controller attacker. Binding the invite to a
pre-shared joiner NodeId would close it and is deferred.

## Secrets at rest
The device key, the network secret, and the originator master key are stored in the OS keystore
(Windows Credential Manager, macOS Keychain, Linux Secret Service) via the `keyring` crate. On a
headless host with no keystore they fall back to `0600` files under `<data_dir>/secrets/`. A
per-secret marker records that a secret is keystore-backed, so if the keystore is temporarily
unavailable the daemon errors clearly rather than regenerating identity (which would silently
evict the device). `network.cbor` holds only non-secret fields (name, subnet, originator pubkey).
This assumes one daemon instance per machine/user account.

On **Android** there is no OS keystore backend (the `keyring` crate has none), so secrets are kept
in the `0600` file fallback under the app's **private internal storage** (`Context.filesDir`),
which is not readable by other apps on a non-rooted device. Android Keystore-backed encryption of
that file is a possible future hardening.

**Custom relay access tokens** (`relays.cbor` in the data dir) are stored in plain form alongside
the other non-secret config, *not* in the keystore. They're a deliberately weaker class of secret:
a relay token only grants the ability to *use* the relay's bandwidth — it can't decrypt, inject, or
impersonate anything (relayed traffic stays end-to-end encrypted QUIC, and identity is the NodeId
key held in the keystore). The data dir is root/LocalSystem-owned on desktop installs, which bounds
who can read it. Keystore-backing the tokens is possible future hardening. Relay settings are
**per-device** and never distributed through the roster, so adding a relay can't be used by one
member to redirect another member's traffic.

A token is never taken as a **command-line argument**: `nullgate-cli relay add <url>` prompts for it
with echo off (reading the terminal directly, or stdin when it is piped). argv is not private — any
other user of the machine can read it out of `ps`, and the shell records the whole command line in
its history file — which would leak the token to precisely the local readers the data dir's
ownership is meant to exclude. The token then travels only over the local IPC socket. The CLI also
*verifies* a token against the relay before saving it (`ProbeRelay`), because storing one the relay
rejects has the same effect as the partition described below, silently.

That isolation has a sharp edge worth stating plainly: **a token-gated relay makes a device
unreachable to peers that don't have the token.** A relay rejects clients without its token (`401`),
and an endpoint advertises only its single *home relay* — so a device homed on your relay is
reachable there and nowhere else. A peer without the token has no relay path to it, and since
hole-punching is relay-coordinated, usually no direct path either. Deploy a custom relay to **every**
member with the same URL and token, or to none: half-and-half is the one configuration that cannot
work, and it fails while the relay itself is perfectly healthy (this is not hypothetical — it
partitioned the author's network for three days). `RelayPolicy::Preferred` keeps the public relays in
the map alongside the custom one specifically to bound this failure; `Only` removes that safety net
by design.

## Privilege boundary (GUI ↔ daemon)
The GUI runs unprivileged and never holds elevation; all privileged work lives in the daemon. The
one place the GUI reaches for privilege is its **Start/Restart service** banner button, and it does
so through the OS's own audited elevation prompt (UAC / polkit / the macOS auth dialog) rather than
any stored credential — the user authenticates each time. The button only starts the platform
service that's already installed; it can't run arbitrary commands.

## Device name is self-asserted (NodeId is the anchor)
The display name members see (the desktop OS hostname; on Android an auto-derived
`"<Manufacturer> <Model> (<suffix>)"`) is written by the client into its own roster `Add` and
presence heartbeats — it is **not** cryptographically bound to identity, on **any** platform, and a
modified client could claim any name. The only non-spoofable identifier is the **NodeId** (the
device's ed25519 public key), which the UI shows and which all admission/roster signatures are
bound to. Android makes the name non-editable in the UI (the hardware serial is unreachable to
normal apps since API 29), which prevents casual spoofing but is not a security boundary — treat
the NodeId, verified via the emoji SAS at join time, as the identity.

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

## Per-device access controls (one-way block, hide)
A Controller/Originator device can refuse inbound access while still reaching others, and can hide
itself from the member list. Both are advertised in the signed presence heartbeat (so other
clients render them) and persisted locally in `device_prefs.cbor`.
- **Disable remote access** is a *real, enforced* one-way block, done in this device's data plane:
  a small connection tracker records the flows this device initiates (on the outbound TUN→mesh
  path), and inbound packets are admitted only if they're return traffic for one of those flows.
  So outbound (you → others) keeps working; unsolicited inbound (others → you) is dropped. Hiding
  implies the block.
- **Hide from member list** is a **presentation courtesy, not a security boundary.** The roster is
  a shared signed log, so a hidden device's entry is still present; standard clients filter it out
  for Peers and Controllers (originators still see it, with a white dot), but a modified client
  could read it. The *access* block is what actually prevents reaching a hidden device — even by an
  originator (who can see it but not connect).

## Taking access away
- **Remove a member** — the originator (or a Controller, for a Peer) signs a `Remove`. It
  propagates to connected peers; each
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
