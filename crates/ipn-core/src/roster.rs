//! The signed membership roster — the security crux of Nullgate.
//!
//! The roster is an append-only set of **signed entries** that fold into the
//! current membership. It is designed to ride on a multi-writer store
//! (iroh-docs) where the write capability *cannot be un-shared* — so a removed
//! member physically retains the ability to append entries. Security therefore
//! does **not** come from controlling who can write; it comes from these
//! application-layer role rules, enforced every time the roster is rebuilt.
//!
//! ## Roles (v2)
//! Every member has a [`Role`] — **Peer** (no admin powers) or **Controller**
//! (may admit/evict Peers). The **originator** is orthogonal: it's whoever holds
//! the exportable master key, and it has full authority regardless of roster
//! role. The fold rules below:
//!
//!   * **`Add`** — admits a member at a stated `role`. Valid iff the signer is the
//!     originator (any role, no invite needed) **or** a current **Controller**
//!     citing the *current* invite nonce for that role's kind (and, if the invite
//!     is single-use, an unconsumed nonce). Blocked while frozen.
//!   * **`Remove`** — valid iff signed by the originator (any target) **or** by a
//!     current Controller whose target is a **Peer**.
//!   * **`SetRole`** — promote/demote in place. **Originator-only.**
//!   * **`SetInvite`** — sets the current join nonce for a kind. A **Peer** invite
//!     may be set by the originator or any Controller; a **Controller** invite is
//!     **originator-only** and always single-use. Latest by `(ts, id)` wins, so
//!     regenerating an invite invalidates the prior code for *new* joins.
//!   * **`Freeze`** — originator-only.
//!
//! Consequences the tests pin down: a non-member (or a Peer) cannot inject
//! members; a removed member's later ops are rejected; a single-use invite admits
//! exactly once; regenerating an invite invalidates the old one.
//!
//! The hard mass-cutoff ("block everyone who ever had access") is **rotate** —
//! minting a fresh network secret + originator key + docs namespace — handled a
//! layer up; this module only enforces the rules of a single network.
//!
//! Identity note: a member's signing key **is** their iroh device key — a NodeId
//! is an ed25519 public key, so the 32-byte NodeId doubles as the verifying key
//! for that member's signatures. The originator master key is a *separate*,
//! exportable ed25519 keypair (so super-admin authority survives device loss).

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// An ed25519 public key: a device's NodeId, or the originator master key.
pub type Id = [u8; 32];

/// A join-invite nonce (carried in the ticket, cited by the admitting `Add`).
pub type Nonce = [u8; 16];

const DOMAIN: &str = "ipn-roster-v2";

/// Entries timestamped more than this far in the future are dropped. Timestamps
/// are member-chosen, so they're only a *hint* for ordering, not a trust anchor.
const MAX_FUTURE_SKEW_MS: u64 = 24 * 60 * 60 * 1000;

/// A member's privilege tier within the roster. The **originator** (master-key
/// holder) is a separate, higher authority and is not represented here.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Role {
    /// No administrative powers (beyond viewing the activity log). The default.
    #[default]
    Peer,
    /// May admit and evict Peers, and generate Peer-level invites.
    Controller,
}

impl Role {
    /// Lower-case wire/display name (`"peer"` / `"controller"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Peer => "peer",
            Role::Controller => "controller",
        }
    }
}

/// Which kind of join a [`Op::SetInvite`] / ticket authorizes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum InviteKind {
    Peer,
    Controller,
}

/// A membership operation. Every variant carries a logical timestamp (`ts`,
/// milliseconds since the Unix epoch) used only to order a concurrent set of
/// entries deterministically; exact wall-clock accuracy is not required.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Admit `node_id` as a member at `role`. `virtual_ip` is the IP the admitter
    /// picked (lowest free at approval time) — honored during the fold so a
    /// device's address is **static** for the life of its membership. `invite_nonce`
    /// ties the admission to a current invite (ignored for originator-signed adds).
    Add {
        node_id: Id,
        hostname: String,
        role: Role,
        virtual_ip: [u8; 4],
        invite_nonce: Nonce,
        ts: u64,
    },
    /// Revoke a single member. Originator (any target) or a Controller (Peer target).
    Remove { node_id: Id, ts: u64 },
    /// Promote/demote a current member in place. Originator-only.
    SetRole { node_id: Id, role: Role, ts: u64 },
    /// Set the current join invite for `kind`. Latest by `(ts, id)` is authoritative.
    SetInvite {
        kind: InviteKind,
        nonce: Nonce,
        single_use: bool,
        ts: u64,
    },
    /// Freeze (or unfreeze) the membership roll. Originator-only.
    Freeze { frozen: bool, ts: u64 },
    /// Set the network's display name. Originator or any current Controller;
    /// last-writer-wins (it's a cosmetic, shared label).
    SetName { name: String, ts: u64 },
}

impl Op {
    /// The logical timestamp carried by this op.
    pub fn ts(&self) -> u64 {
        match self {
            Op::Add { ts, .. }
            | Op::Remove { ts, .. }
            | Op::SetRole { ts, .. }
            | Op::SetInvite { ts, .. }
            | Op::Freeze { ts, .. }
            | Op::SetName { ts, .. } => *ts,
        }
    }
}

/// A signed roster entry. `signature` is over the canonical bytes of
/// `(DOMAIN, network_id, signer, op)`, so it binds the op to the claimed signer
/// and to this specific network.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub network_id: Id,
    pub signer: Id,
    pub op: Op,
    pub signature: Vec<u8>,
}

impl Entry {
    /// Content address of this entry (used for dedup + deterministic tie-break).
    pub fn id(&self) -> [u8; 32] {
        let mut buf = Vec::new();
        ciborium::into_writer(&(&self.network_id, &self.signer, &self.op, &self.signature), &mut buf)
            .expect("serialize entry");
        *blake3::hash(&buf).as_bytes()
    }

    /// Verify the entry's signature against its claimed signer. This checks
    /// authenticity only — *authorization* (role rules) is applied in
    /// [`Roster::build`].
    pub fn verify_signature(&self) -> bool {
        let Ok(sig_arr): Result<[u8; 64], _> = self.signature.as_slice().try_into() else {
            return false;
        };
        let sig = Signature::from_bytes(&sig_arr);
        let Ok(vk) = VerifyingKey::from_bytes(&self.signer) else {
            return false;
        };
        vk.verify_strict(&signing_bytes(&self.network_id, &self.signer, &self.op), &sig)
            .is_ok()
    }
}

/// Canonical bytes that get signed for an entry.
fn signing_bytes(network_id: &Id, signer: &Id, op: &Op) -> Vec<u8> {
    #[derive(Serialize)]
    struct View<'a> {
        domain: &'static str,
        network_id: &'a Id,
        signer: &'a Id,
        op: &'a Op,
    }
    let view = View {
        domain: DOMAIN,
        network_id,
        signer,
        op,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&view, &mut buf).expect("serialize signing view");
    buf
}

/// Sign an op, producing a transmittable [`Entry`]. `signing_key` is the
/// member's device key (for `Add`/`SetInvite`) or the originator master key (for
/// `Remove`/`Freeze`/`SetRole`/originator-direct ops).
pub fn sign(network_id: Id, signing_key: &SigningKey, op: Op) -> Entry {
    let signer = signing_key.verifying_key().to_bytes();
    let sig = signing_key.sign(&signing_bytes(&network_id, &signer, &op));
    Entry {
        network_id,
        signer,
        op,
        signature: sig.to_bytes().to_vec(),
    }
}

/// Network parameters needed to evaluate the roster.
#[derive(Clone, Debug)]
pub struct Config {
    /// Stable identifier for this network (domain separation across networks).
    pub network_id: Id,
    /// The originator master public key — the top authority and the bootstrap
    /// signer of the first member.
    pub originator_id: Id,
    /// The virtual subnet (a /24, e.g. `10.99.0.0`). Member IPs live within it.
    pub subnet: Ipv4Addr,
}

/// A current member of the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub hostname: String,
    pub role: Role,
    pub virtual_ip: Ipv4Addr,
    /// Which key vouched this member in (a current member, or the originator).
    pub added_by: Id,
}

/// The folded current state of the roster.
#[derive(Clone, Debug, Default)]
pub struct Roster {
    members: BTreeMap<Id, Member>,
    frozen: bool,
    /// Shared display name (latest authorized `SetName`), if any.
    name: Option<String>,
    /// Current Peer / Controller invite (latest authorized `SetInvite` per kind):
    /// `(nonce, single_use)`.
    peer_invite: Option<(Nonce, bool)>,
    controller_invite: Option<(Nonce, bool)>,
}

impl Roster {
    /// Fold a set of entries into the current membership, enforcing all role
    /// rules. Entries with bad signatures, the wrong network, or insufficient
    /// authority are silently dropped — a hostile writer cannot corrupt the
    /// outcome, only waste space.
    pub fn build(cfg: &Config, entries: &[Entry]) -> Roster {
        // 1. Keep only authentic entries for this network. Dedup by content id.
        //    Drop entries timestamped implausibly far in the future.
        let ceiling = now_ms().saturating_add(MAX_FUTURE_SKEW_MS);
        let mut valid: BTreeMap<[u8; 32], &Entry> = BTreeMap::new();
        for e in entries {
            if e.network_id == cfg.network_id && e.op.ts() <= ceiling && e.verify_signature() {
                valid.insert(e.id(), e);
            }
        }

        // 2. Deterministic order: by logical timestamp, then content id.
        let mut ordered: Vec<&Entry> = valid.values().copied().collect();
        ordered.sort_by(|a, b| a.op.ts().cmp(&b.op.ts()).then_with(|| a.id().cmp(&b.id())));

        // 3. Fold, applying authorization at each step against the state so far.
        let mut roster = Roster::default();
        let mut admitted_ts: BTreeMap<Id, u64> = BTreeMap::new();
        // Invite nonces already spent by an accepted single-use admission.
        let mut consumed: BTreeSet<Nonce> = BTreeSet::new();
        let orig = cfg.originator_id;

        for e in ordered {
            let ts = e.op.ts();
            let is_orig = e.signer == orig;
            // A signer's current tier (None if not a current member).
            let signer_role = roster.members.get(&e.signer).map(|m| m.role);
            let is_controller = signer_role == Some(Role::Controller);

            match &e.op {
                Op::Freeze { frozen, .. } => {
                    if is_orig {
                        roster.frozen = *frozen;
                    }
                }
                Op::Remove { node_id, .. } => {
                    let target_role = roster.members.get(node_id).map(|m| m.role);
                    let authorized = is_orig
                        || (is_controller && target_role == Some(Role::Peer));
                    if authorized {
                        roster.members.remove(node_id);
                        admitted_ts.remove(node_id);
                    }
                }
                Op::SetRole { node_id, role, .. } => {
                    // In-place promote/demote: originator-only, member must exist.
                    if is_orig {
                        if let Some(m) = roster.members.get_mut(node_id) {
                            m.role = *role;
                        }
                    }
                }
                Op::SetInvite {
                    kind,
                    nonce,
                    single_use,
                    ..
                } => match kind {
                    InviteKind::Peer => {
                        if is_orig || is_controller {
                            roster.peer_invite = Some((*nonce, *single_use));
                        }
                    }
                    InviteKind::Controller => {
                        // Controller invites are originator-only and single-use.
                        if is_orig {
                            roster.controller_invite = Some((*nonce, true));
                        }
                    }
                },
                Op::SetName { name, .. } => {
                    if is_orig || is_controller {
                        roster.name = Some(name.clone());
                    }
                }
                Op::Add {
                    node_id,
                    hostname,
                    role,
                    virtual_ip,
                    invite_nonce,
                    ..
                } => {
                    if roster.frozen {
                        continue; // no adds while frozen, even by the originator
                    }
                    // (a) Approver authorization.
                    let approver_ok = is_orig
                        || (is_controller
                            && admitted_ts.get(&e.signer).is_some_and(|t| ts >= *t));
                    if !approver_ok {
                        continue;
                    }
                    // (b) Invite gate. The originator may add directly (no nonce);
                    //     a Controller must cite the *current*, matching-kind,
                    //     not-yet-consumed invite for the role being granted.
                    let mut consume: Option<Nonce> = None;
                    if !is_orig {
                        let current = match role {
                            Role::Peer => roster.peer_invite,
                            Role::Controller => roster.controller_invite,
                        };
                        let Some((cur_nonce, single_use)) = current else {
                            continue; // no current invite for this kind
                        };
                        if *invite_nonce != cur_nonce {
                            continue; // stale / wrong code → rejected
                        }
                        if single_use {
                            if consumed.contains(&cur_nonce) {
                                continue; // already spent
                            }
                            consume = Some(cur_nonce);
                        }
                    }
                    if let Some(n) = consume {
                        consumed.insert(n);
                    }
                    roster.members.insert(
                        *node_id,
                        Member {
                            hostname: hostname.clone(),
                            role: *role,
                            virtual_ip: Ipv4Addr::from(*virtual_ip), // resolved below
                            added_by: e.signer,
                        },
                    );
                    admitted_ts.insert(*node_id, ts);
                }
            }
        }

        // Resolve final virtual IPs: honor each member's recorded address, probing
        // forward only on a genuine collision. Processed in admission order so a
        // later join never displaces an earlier member's IP.
        assign_ips(&mut roster.members, &admitted_ts, cfg.subnet);
        roster
    }

    /// The shared display name, if any member has set one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn is_member(&self, id: &Id) -> bool {
        self.members.contains_key(id)
    }

    pub fn member(&self, id: &Id) -> Option<&Member> {
        self.members.get(id)
    }

    /// A member's role, or `Peer` if not a member.
    pub fn role(&self, id: &Id) -> Role {
        self.members.get(id).map(|m| m.role).unwrap_or(Role::Peer)
    }

    pub fn members(&self) -> impl Iterator<Item = (&Id, &Member)> {
        self.members.iter()
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// The current invite `(nonce, single_use)` for `kind`, if one is set.
    pub fn current_invite(&self, kind: InviteKind) -> Option<(Nonce, bool)> {
        match kind {
            InviteKind::Peer => self.peer_invite,
            InviteKind::Controller => self.controller_invite,
        }
    }

    /// The lowest free host address in `subnet` (2..=254), or `.254` if the /24 is
    /// somehow full. Used by an admitter to pick a static IP for a new member.
    pub fn lowest_free_host(&self, subnet: Ipv4Addr) -> Ipv4Addr {
        let base = subnet.octets();
        let taken: BTreeSet<u8> = self
            .members
            .values()
            .map(|m| m.virtual_ip.octets()[3])
            .collect();
        let host = (2u8..=254).find(|h| !taken.contains(h)).unwrap_or(254);
        Ipv4Addr::new(base[0], base[1], base[2], host)
    }
}

/// Resolve each member's final virtual IP, honoring the address recorded in its
/// `Add` (so IPs are stable). Members are processed in admission order
/// (`admitted_ts`, then NodeId); each takes its recorded host if it's a valid,
/// free host, otherwise it probes forward to the next free one. Because every
/// surviving member is assigned from its *own* record, removing another member
/// never shifts it.
fn assign_ips(members: &mut BTreeMap<Id, Member>, admitted_ts: &BTreeMap<Id, u64>, subnet: Ipv4Addr) {
    let base = subnet.octets();
    let mut order: Vec<(u64, Id, u8)> = members
        .iter()
        .map(|(id, m)| {
            (
                admitted_ts.get(id).copied().unwrap_or(0),
                *id,
                m.virtual_ip.octets()[3],
            )
        })
        .collect();
    order.sort_by_key(|(ts, id, _)| (*ts, *id));

    let mut taken: BTreeSet<u8> = BTreeSet::new();
    for (_, id, recorded) in order {
        let mut host = if (2..=254).contains(&recorded) { recorded } else { 2 };
        let mut tries = 0;
        while taken.contains(&host) && tries < 253 {
            host = if host >= 254 { 2 } else { host + 1 };
            tries += 1;
        }
        taken.insert(host);
        if let Some(m) = members.get_mut(&id) {
            m.virtual_ip = Ipv4Addr::new(base[0], base[1], base[2], host);
        }
    }
}

/// Current time in milliseconds since the Unix epoch (for real entry creation).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn id(k: &SigningKey) -> Id {
        k.verifying_key().to_bytes()
    }
    const SUBNET: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 0);
    const NET: Id = [9u8; 32];

    /// A Peer-kind invite nonce derived from a byte (for tests).
    fn nonce(b: u8) -> Nonce {
        [b; 16]
    }

    fn add(
        net: Id,
        signer: &SigningKey,
        node: &SigningKey,
        host: u8,
        role: Role,
        inv: Nonce,
        ts: u64,
    ) -> Entry {
        sign(
            net,
            signer,
            Op::Add {
                node_id: id(node),
                hostname: "host".into(),
                role,
                virtual_ip: [10, 99, 0, host],
                invite_nonce: inv,
                ts,
            },
        )
    }

    /// Standard setup: originator master key `om`, originator device `devo`
    /// (bootstrapped by the master key as the genesis Controller member), plus a
    /// current Peer invite so Controllers can admit Peers.
    fn setup() -> (Config, SigningKey, SigningKey, Vec<Entry>) {
        let om = key(1); // originator master (exportable authority)
        let devo = key(2); // originator's device (a Controller member)
        let cfg = Config {
            network_id: NET,
            originator_id: id(&om),
            subnet: SUBNET,
        };
        let genesis = add(NET, &om, &devo, 2, Role::Controller, nonce(0), 1);
        let peer_inv = sign(
            NET,
            &om,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce: nonce(7),
                single_use: false,
                ts: 1,
            },
        );
        (cfg, om, devo, vec![genesis, peer_inv])
    }

    #[test]
    fn genesis_member_is_controller() {
        let (cfg, _om, devo, entries) = setup();
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&devo)));
        assert_eq!(r.role(&id(&devo)), Role::Controller);
        assert_eq!(r.member(&id(&devo)).unwrap().virtual_ip, Ipv4Addr::new(10, 99, 0, 2));
    }

    #[test]
    fn controller_admits_peer_with_current_invite() {
        let (cfg, _om, devo, mut entries) = setup();
        let laptop = key(3);
        entries.push(add(NET, &devo, &laptop, 3, Role::Peer, nonce(7), 2));
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&laptop)));
        assert_eq!(r.role(&id(&laptop)), Role::Peer);
    }

    #[test]
    fn add_with_stale_invite_is_rejected() {
        let (cfg, _om, devo, mut entries) = setup();
        let laptop = key(3);
        // Cite a nonce that isn't the current Peer invite (7).
        entries.push(add(NET, &devo, &laptop, 3, Role::Peer, nonce(99), 2));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&laptop)), "stale invite must not admit");
    }

    #[test]
    fn regenerating_peer_invite_invalidates_old() {
        let (cfg, om, devo, mut entries) = setup();
        // Originator regenerates the Peer invite (new nonce supersedes #7).
        entries.push(sign(
            NET,
            &om,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce: nonce(8),
                single_use: false,
                ts: 5,
            },
        ));
        let stale = key(3);
        let fresh = key(4);
        // A join citing the OLD nonce after the regeneration is rejected...
        entries.push(add(NET, &devo, &stale, 3, Role::Peer, nonce(7), 6));
        // ...one citing the new nonce is accepted.
        entries.push(add(NET, &devo, &fresh, 4, Role::Peer, nonce(8), 7));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&stale)), "old code invalidated");
        assert!(r.is_member(&id(&fresh)), "new code works");
    }

    #[test]
    fn peer_cannot_admit() {
        let (cfg, _om, devo, mut entries) = setup();
        // devo (Controller) admits a Peer `p`.
        let p = key(3);
        entries.push(add(NET, &devo, &p, 3, Role::Peer, nonce(7), 2));
        // The Peer `p` then tries to admit `victim` citing the current invite.
        let victim = key(4);
        entries.push(add(NET, &p, &victim, 4, Role::Peer, nonce(7), 3));
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&p)));
        assert!(!r.is_member(&id(&victim)), "a Peer must not be able to admit");
    }

    #[test]
    fn non_member_cannot_admit() {
        let (cfg, _om, _devo, mut entries) = setup();
        let stranger = key(50);
        let victim = key(51);
        entries.push(add(NET, &stranger, &victim, 5, Role::Peer, nonce(7), 2));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&victim)));
        assert!(!r.is_member(&id(&stranger)));
    }

    #[test]
    fn controller_removes_peer_but_not_controller() {
        let (cfg, om, devo, mut entries) = setup();
        // Promote a second device `c2` to Controller (originator SetRole), and add
        // a Peer `p`.
        let c2 = key(3);
        let p = key(4);
        entries.push(add(NET, &devo, &c2, 3, Role::Peer, nonce(7), 2));
        entries.push(sign(NET, &om, Op::SetRole { node_id: id(&c2), role: Role::Controller, ts: 3 }));
        entries.push(add(NET, &devo, &p, 4, Role::Peer, nonce(7), 4));

        // c2 (Controller) removes the Peer -> gone.
        let mut e1 = entries.clone();
        e1.push(sign(NET, &c2, Op::Remove { node_id: id(&p), ts: 5 }));
        assert!(!Roster::build(&cfg, &e1).is_member(&id(&p)), "Controller evicts Peer");

        // c2 (Controller) tries to remove devo (a Controller) -> ignored.
        let mut e2 = entries.clone();
        e2.push(sign(NET, &c2, Op::Remove { node_id: id(&devo), ts: 5 }));
        assert!(Roster::build(&cfg, &e2).is_member(&id(&devo)), "Controller can't evict Controller");
    }

    #[test]
    fn only_originator_removes_controller() {
        let (cfg, om, devo, mut entries) = setup();
        let c2 = key(3);
        entries.push(add(NET, &devo, &c2, 3, Role::Peer, nonce(7), 2));
        entries.push(sign(NET, &om, Op::SetRole { node_id: id(&c2), role: Role::Controller, ts: 3 }));
        // Originator removes the Controller -> gone.
        entries.push(sign(NET, &om, Op::Remove { node_id: id(&c2), ts: 4 }));
        assert!(!Roster::build(&cfg, &entries).is_member(&id(&c2)));
    }

    #[test]
    fn controller_invite_is_originator_only_and_single_use() {
        let (cfg, om, devo, mut entries) = setup();
        // A Controller (devo) trying to set a Controller invite is ignored.
        entries.push(sign(
            NET,
            &devo,
            Op::SetInvite { kind: InviteKind::Controller, nonce: nonce(20), single_use: false, ts: 2 },
        ));
        let a = key(3);
        entries.push(add(NET, &devo, &a, 3, Role::Controller, nonce(20), 3));
        assert!(
            !Roster::build(&cfg, &entries).is_member(&id(&a)),
            "Controller-set Controller invite must not work"
        );

        // Originator issues a Controller invite; it admits exactly one Controller.
        entries.push(sign(
            NET,
            &om,
            Op::SetInvite { kind: InviteKind::Controller, nonce: nonce(21), single_use: false, ts: 4 },
        ));
        let first = key(5);
        let second = key(6);
        entries.push(add(NET, &devo, &first, 5, Role::Controller, nonce(21), 5));
        entries.push(add(NET, &devo, &second, 6, Role::Controller, nonce(21), 6));
        let r = Roster::build(&cfg, &entries);
        assert_eq!(r.role(&id(&first)), Role::Controller, "first Controller admitted");
        assert!(!r.is_member(&id(&second)), "Controller invite is single-use");
    }

    #[test]
    fn single_use_peer_invite_consumed_once() {
        let (cfg, om, devo, mut entries) = setup();
        entries.push(sign(
            NET,
            &om,
            Op::SetInvite { kind: InviteKind::Peer, nonce: nonce(30), single_use: true, ts: 4 },
        ));
        let a = key(3);
        let b = key(4);
        entries.push(add(NET, &devo, &a, 3, Role::Peer, nonce(30), 5));
        entries.push(add(NET, &devo, &b, 4, Role::Peer, nonce(30), 6));
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&a)), "first single-use join admitted");
        assert!(!r.is_member(&id(&b)), "second single-use join rejected");
    }

    #[test]
    fn role_cannot_be_upgraded_via_peer_nonce() {
        let (cfg, _om, devo, mut entries) = setup();
        // Claim role=Controller while citing the *Peer* invite nonce -> rejected
        // (kind mismatch; there is no current Controller invite).
        let x = key(3);
        entries.push(add(NET, &devo, &x, 3, Role::Controller, nonce(7), 2));
        assert!(!Roster::build(&cfg, &entries).is_member(&id(&x)));
    }

    #[test]
    fn removed_member_cannot_forge() {
        let (cfg, om, devo, mut entries) = setup();
        let laptop = key(3);
        entries.push(add(NET, &devo, &laptop, 3, Role::Peer, nonce(7), 2));
        // Originator removes the laptop at ts=3.
        entries.push(sign(NET, &om, Op::Remove { node_id: id(&laptop), ts: 3 }));
        // The removed laptop (now a non-member Peer) tries to admit + remove.
        let backdoor = key(60);
        entries.push(add(NET, &laptop, &backdoor, 4, Role::Peer, nonce(7), 4));
        entries.push(sign(NET, &laptop, Op::Remove { node_id: id(&devo), ts: 5 }));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&laptop)), "removed member stays out");
        assert!(!r.is_member(&id(&backdoor)), "forged add rejected");
        assert!(r.is_member(&id(&devo)), "forged remove ignored");
    }

    #[test]
    fn freeze_blocks_adds_until_unfrozen() {
        let (cfg, om, devo, base) = setup();
        let q = key(4);

        let mut frozen = base.clone();
        frozen.push(sign(NET, &om, Op::Freeze { frozen: true, ts: 3 }));
        frozen.push(add(NET, &devo, &q, 3, Role::Peer, nonce(7), 4));
        let r = Roster::build(&cfg, &frozen);
        assert!(r.frozen());
        assert!(!r.is_member(&id(&q)), "add blocked while frozen");

        let mut thawed = frozen.clone();
        thawed.push(sign(NET, &om, Op::Freeze { frozen: false, ts: 5 }));
        thawed.push(add(NET, &devo, &q, 3, Role::Peer, nonce(7), 6));
        let r = Roster::build(&cfg, &thawed);
        assert!(!r.frozen());
        assert!(r.is_member(&id(&q)), "add allowed after unfreeze");
    }

    #[test]
    fn tampered_signature_is_dropped() {
        let (cfg, _om, devo, _e) = setup();
        let mut bad = add(NET, &devo, &key(3), 3, Role::Peer, nonce(7), 2);
        bad.signature[0] ^= 0xff;
        assert!(!bad.verify_signature());
        assert!(Roster::build(&cfg, std::slice::from_ref(&bad)).is_empty());
    }

    #[test]
    fn wrong_network_id_is_dropped() {
        let (cfg, om, devo, _e) = setup();
        let foreign = add([7u8; 32], &om, &devo, 2, Role::Controller, nonce(0), 1);
        assert!(Roster::build(&cfg, std::slice::from_ref(&foreign)).is_empty());
    }

    #[test]
    fn ips_are_static_across_other_joins_and_leaves() {
        let (cfg, om, devo, mut entries) = setup();
        let a = key(10);
        let b = key(11);
        // a joins (claims .3), b joins (claims .4).
        entries.push(add(NET, &devo, &a, 3, Role::Peer, nonce(7), 2));
        entries.push(add(NET, &devo, &b, 4, Role::Peer, nonce(7), 3));
        let r1 = Roster::build(&cfg, &entries);
        let a_ip = r1.member(&id(&a)).unwrap().virtual_ip;
        assert_eq!(a_ip, Ipv4Addr::new(10, 99, 0, 3));

        // Remove b. a's IP must not move.
        entries.push(sign(NET, &om, Op::Remove { node_id: id(&b), ts: 4 }));
        let r2 = Roster::build(&cfg, &entries);
        assert_eq!(r2.member(&id(&a)).unwrap().virtual_ip, a_ip, "IP stays static");
    }

    #[test]
    fn concurrent_same_ip_claims_resolve_to_distinct() {
        // Two admitters concurrently pick the same host for two joiners; the fold
        // resolves the collision deterministically so they still get distinct IPs.
        let (cfg, om, devo, mut entries) = setup();
        let c2 = key(3);
        entries.push(add(NET, &devo, &c2, 3, Role::Peer, nonce(7), 2));
        entries.push(sign(NET, &om, Op::SetRole { node_id: id(&c2), role: Role::Controller, ts: 3 }));
        let a = key(40);
        let b = key(41);
        // Both admitted at ts=5 claiming .9.
        entries.push(add(NET, &devo, &a, 9, Role::Peer, nonce(7), 5));
        entries.push(add(NET, &c2, &b, 9, Role::Peer, nonce(7), 5));
        let r = Roster::build(&cfg, &entries);
        assert_ne!(
            r.member(&id(&a)).unwrap().virtual_ip,
            r.member(&id(&b)).unwrap().virtual_ip,
            "collision resolved to distinct IPs"
        );
    }

    #[test]
    fn set_name_last_writer_wins_and_requires_controller() {
        let (cfg, _om, devo, mut entries) = setup();
        let outsider = key(50);
        entries.push(sign(NET, &devo, Op::SetName { name: "Home".into(), ts: 10 }));
        entries.push(sign(NET, &devo, Op::SetName { name: "Lab".into(), ts: 20 }));
        entries.push(sign(NET, &outsider, Op::SetName { name: "Hacked".into(), ts: 30 }));
        let r = Roster::build(&cfg, &entries);
        assert_eq!(r.name(), Some("Lab"));
    }
}
