//! The signed membership roster — the security crux of Nullgate.
//!
//! The roster is an append-only set of **signed entries** that fold into the
//! current membership. It is designed to ride on a multi-writer store
//! (iroh-docs) where the write capability *cannot be un-shared* — so a removed
//! member physically retains the ability to append entries. Security therefore
//! does **not** come from controlling who can write; it comes from these
//! application-layer role rules, enforced every time the roster is rebuilt:
//!
//!   * **`Add`** — a member may vouch for a joiner (web-of-trust). Valid iff the
//!     signer is a *current member* (or the originator) **and** the roster is not
//!     frozen at that point in time.
//!   * **`Remove`** — valid iff signed by the **originator master key**.
//!   * **`Freeze`** — valid iff signed by the **originator master key**.
//!
//! Consequences that the tests below pin down:
//!   * A non-member cannot inject members.
//!   * A *removed* member's later `Add`s are rejected (they're no longer a
//!     current member), and they can never sign `Remove`/`Freeze`.
//!   * Freezing the roster blocks all further adds until it is unfrozen.
//!
//! The hard mass-cutoff ("block everyone who ever had access") is **rotate** —
//! minting a fresh network secret + originator key + docs namespace — handled a
//! layer up; this module only enforces the rules of a single network.
//!
//! Identity note: a member's signing key **is** their iroh device key — a NodeId
//! is an ed25519 public key, so the 32-byte NodeId doubles as the verifying key
//! for that member's signatures. The originator master key is a *separate*,
//! exportable ed25519 keypair (so super-admin authority survives device loss).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// An ed25519 public key: a device's NodeId, or the originator master key.
pub type Id = [u8; 32];

const DOMAIN: &str = "ipn-roster-v1";

/// Entries timestamped more than this far in the future are dropped. Timestamps
/// are member-chosen, so they're only a *hint* for ordering, not a trust anchor.
///
/// Residual (documented, not fully fixed here): a current member could still
/// backdate an `Add` into a past *unfrozen* window to slip a device past a freeze.
/// Fully preventing that needs causal ordering (a hash-linked DAG / version
/// vectors) — deferred. The backstop is that such an attacker is already a trusted
/// member, and the originator can remove the device or rotate the secret.
const MAX_FUTURE_SKEW_MS: u64 = 24 * 60 * 60 * 1000;

/// A membership operation. Every variant carries a logical timestamp (`ts`,
/// milliseconds since the Unix epoch) used only to order a concurrent set of
/// entries deterministically; exact wall-clock accuracy is not required.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Admit `node_id` as a member (after out-of-band SAS verification). The
    /// member's virtual IP is NOT chosen here — it's derived deterministically
    /// from the NodeId during [`Roster::build`], so concurrent approvals by
    /// different members can never assign the same address (see `assign_ips`).
    Add {
        node_id: Id,
        hostname: String,
        ts: u64,
    },
    /// Revoke a single member. Originator-only.
    Remove { node_id: Id, ts: u64 },
    /// Freeze (or unfreeze) the membership roll. Originator-only.
    Freeze { frozen: bool, ts: u64 },
    /// Set the network's display name. Any current member may set it;
    /// last-writer-wins (it's a cosmetic, shared label).
    SetName { name: String, ts: u64 },
}

impl Op {
    fn ts(&self) -> u64 {
        match self {
            Op::Add { ts, .. }
            | Op::Remove { ts, .. }
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
/// member's device key (for `Add`) or the originator master key (for
/// `Remove`/`Freeze`).
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
    /// The originator master public key — the sole authority for removals/freeze
    /// and the bootstrap signer of the first member.
    pub originator_id: Id,
    /// The virtual subnet (a /24, e.g. `10.99.0.0`). Member IPs are assigned
    /// deterministically within it during [`Roster::build`].
    pub subnet: Ipv4Addr,
}

/// A current member of the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub hostname: String,
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
}

impl Roster {
    /// Fold a set of entries into the current membership, enforcing all role
    /// rules. Entries with bad signatures, the wrong network, or insufficient
    /// authority are silently dropped — a hostile writer cannot corrupt the
    /// outcome, only waste space.
    pub fn build(cfg: &Config, entries: &[Entry]) -> Roster {
        // 1. Keep only authentic entries for this network. Dedup by content id.
        //    Drop entries timestamped implausibly far in the future (anti
        //    forward-dating; bounds the ordering games a member can play).
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
        //    `admitted_ts` records when each member was admitted, so a member can't
        //    sign an Add backdated to *before it joined* (a cheap ordering defense;
        //    see the note on `MAX_FUTURE_SKEW_MS` for the residual).
        let mut roster = Roster::default();
        let mut admitted_ts: BTreeMap<Id, u64> = BTreeMap::new();
        for e in ordered {
            let ts = e.op.ts();
            match &e.op {
                Op::Freeze { frozen, .. } => {
                    if e.signer == cfg.originator_id {
                        roster.frozen = *frozen;
                    }
                }
                Op::Remove { node_id, .. } => {
                    if e.signer == cfg.originator_id {
                        roster.members.remove(node_id);
                        admitted_ts.remove(node_id);
                    }
                }
                Op::SetName { name, .. } => {
                    // Any current member (or the originator) may rename; entries are
                    // processed in (ts, id) order, so the last authorized one wins.
                    if e.signer == cfg.originator_id || roster.members.contains_key(&e.signer) {
                        roster.name = Some(name.clone());
                    }
                }
                Op::Add {
                    node_id, hostname, ..
                } => {
                    // No adds while frozen — including by the originator; the
                    // switch must be flipped back first.
                    if roster.frozen {
                        continue;
                    }
                    // The originator may always vouch; a member may vouch only while
                    // it's a current member and not with a timestamp earlier than its
                    // own admission.
                    let authorized = e.signer == cfg.originator_id
                        || (roster.members.contains_key(&e.signer)
                            && admitted_ts.get(&e.signer).is_some_and(|t| ts >= *t));
                    if authorized {
                        roster.members.insert(
                            *node_id,
                            Member {
                                hostname: hostname.clone(),
                                virtual_ip: Ipv4Addr::UNSPECIFIED, // assigned below
                                added_by: e.signer,
                            },
                        );
                        admitted_ts.insert(*node_id, ts);
                    }
                }
            }
        }
        // Assign each member a stable, collision-free virtual IP derived from its
        // NodeId. This is a pure function of the member SET, so every node computes
        // the identical mapping and no two members ever share an address — which is
        // what eliminates the concurrent-approval IP race.
        assign_ips(&mut roster.members, cfg.subnet);
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
}

/// Deterministically assign each member a virtual IP in `subnet` (a /24), keyed
/// by NodeId so the result is identical on every node and independent of who
/// approved whom — there is no IP to race over.
///
/// Each member's preferred host is `2 + blake3(node_id) mod 253` (hosts 2..=254,
/// reserving .0/.1/.255). Members are processed in NodeId order; on the rare hash
/// collision the later (larger) NodeId probes forward to the next free host. So a
/// device's IP is effectively permanent and only ever moves on an actual collision.
fn assign_ips(members: &mut BTreeMap<Id, Member>, subnet: Ipv4Addr) {
    let base = subnet.octets();
    let mut taken: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    // BTreeMap iterates in NodeId order → deterministic probe outcomes.
    for (id, member) in members.iter_mut() {
        let h = u32::from_be_bytes([id[0], id[1], id[2], id[3]]);
        let mut host = (2 + (h % 253)) as u8; // 2..=254
        let mut tries = 0;
        while taken.contains(&host) && tries < 253 {
            host = if host >= 254 { 2 } else { host + 1 };
            tries += 1;
        }
        taken.insert(host);
        member.virtual_ip = Ipv4Addr::new(base[0], base[1], base[2], host);
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

    /// Standard setup: originator master key `om`, originator device `devo`
    /// (bootstrapped by the master key as the genesis member).
    fn setup() -> (Config, SigningKey, SigningKey, Vec<Entry>) {
        let om = key(1); // originator master (exportable authority)
        let devo = key(2); // originator's device (a normal member)
        let net = [9u8; 32];
        let cfg = Config {
            network_id: net,
            originator_id: id(&om),
            subnet: Ipv4Addr::new(10, 99, 0, 0),
        };
        let genesis = sign(
            net,
            &om,
            Op::Add {
                node_id: id(&devo),
                hostname: "originator-pc".into(),                ts: 1,
            },
        );
        (cfg, om, devo, vec![genesis])
    }

    #[test]
    fn genesis_member_is_admitted() {
        let (cfg, _om, devo, entries) = setup();
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&devo)));
        assert_eq!(r.len(), 1);
        assert_eq!(r.member(&id(&devo)).unwrap().hostname, "originator-pc");
    }

    #[test]
    fn web_of_trust_member_can_admit_member() {
        let (cfg, _om, devo, mut entries) = setup();
        let laptop = key(3);
        // The originator's device (a member) vouches for the laptop.
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&laptop),
                hostname: "laptop".into(),                ts: 2,
            },
        ));
        let r = Roster::build(&cfg, &entries);
        assert!(r.is_member(&id(&laptop)));
        assert_eq!(r.member(&id(&laptop)).unwrap().added_by, id(&devo));
    }

    #[test]
    fn non_member_cannot_admit() {
        let (cfg, _om, _devo, mut entries) = setup();
        let stranger = key(50); // not a member, not the originator
        let victim = key(51);
        entries.push(sign(
            cfg.network_id,
            &stranger,
            Op::Add {
                node_id: id(&victim),
                hostname: "evil".into(),                ts: 2,
            },
        ));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&victim)));
        assert!(!r.is_member(&id(&stranger)));
    }

    #[test]
    fn only_originator_removes() {
        let (cfg, om, devo, mut entries) = setup();
        let laptop = key(3);
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&laptop),
                hostname: "laptop".into(),                ts: 2,
            },
        ));
        // A non-originator member tries to remove the laptop -> ignored.
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Remove {
                node_id: id(&laptop),
                ts: 3,
            },
        ));
        assert!(Roster::build(&cfg, &entries).is_member(&id(&laptop)));

        // The originator master key removes it -> gone.
        entries.push(sign(
            cfg.network_id,
            &om,
            Op::Remove {
                node_id: id(&laptop),
                ts: 4,
            },
        ));
        assert!(!Roster::build(&cfg, &entries).is_member(&id(&laptop)));
    }

    #[test]
    fn removed_member_cannot_forge() {
        // The crux: even though a removed member still holds the docs write-cap,
        // their later Adds are rejected and they can't sign Remove/Freeze.
        let (cfg, om, devo, mut entries) = setup();
        let laptop = key(3);
        let attacker_target = key(60);
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&laptop),
                hostname: "laptop".into(),                ts: 2,
            },
        ));
        // Originator removes the laptop at ts=3.
        entries.push(sign(
            cfg.network_id,
            &om,
            Op::Remove {
                node_id: id(&laptop),
                ts: 3,
            },
        ));
        // Removed laptop tries to (a) admit a new member and (b) freeze + remove
        // the originator's device, all at later timestamps.
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Add {
                node_id: id(&attacker_target),
                hostname: "backdoor".into(),                ts: 4,
            },
        ));
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Remove {
                node_id: id(&devo),
                ts: 5,
            },
        ));
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Freeze {
                frozen: true,
                ts: 6,
            },
        ));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&laptop)), "removed member stays out");
        assert!(!r.is_member(&id(&attacker_target)), "forged add rejected");
        assert!(r.is_member(&id(&devo)), "forged remove ignored");
        assert!(!r.frozen(), "forged freeze ignored");
    }

    #[test]
    fn freeze_blocks_adds_until_unfrozen() {
        let (cfg, om, devo, base) = setup();
        let q = key(4);

        // Frozen at ts=3, then an add at ts=4 -> rejected.
        let mut frozen = base.clone();
        frozen.push(sign(
            cfg.network_id,
            &om,
            Op::Freeze {
                frozen: true,
                ts: 3,
            },
        ));
        frozen.push(sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&q),
                hostname: "q".into(),                ts: 4,
            },
        ));
        let r = Roster::build(&cfg, &frozen);
        assert!(r.frozen());
        assert!(!r.is_member(&id(&q)), "add blocked while frozen");

        // Unfreeze at ts=5, re-add at ts=6 -> accepted.
        let mut thawed = frozen.clone();
        thawed.push(sign(
            cfg.network_id,
            &om,
            Op::Freeze {
                frozen: false,
                ts: 5,
            },
        ));
        thawed.push(sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&q),
                hostname: "q".into(),                ts: 6,
            },
        ));
        let r = Roster::build(&cfg, &thawed);
        assert!(!r.frozen());
        assert!(r.is_member(&id(&q)), "add allowed after unfreeze");
    }

    #[test]
    fn tampered_signature_is_dropped() {
        let (cfg, _om, devo, _entries) = setup();
        let mut bad = sign(
            cfg.network_id,
            &devo,
            Op::Add {
                node_id: id(&key(3)),
                hostname: "x".into(),                ts: 2,
            },
        );
        bad.signature[0] ^= 0xff; // corrupt
        assert!(!bad.verify_signature());
        let r = Roster::build(&cfg, std::slice::from_ref(&bad));
        assert!(r.is_empty());
    }

    #[test]
    fn wrong_network_id_is_dropped() {
        let (cfg, om, devo, _e) = setup();
        // Genesis signed for a DIFFERENT network must not count here.
        let foreign = sign(
            [7u8; 32],
            &om,
            Op::Add {
                node_id: id(&devo),
                hostname: "x".into(),                ts: 1,
            },
        );
        let r = Roster::build(&cfg, std::slice::from_ref(&foreign));
        assert!(r.is_empty());
    }

    #[test]
    fn ips_are_unique_in_subnet_and_deterministic() {
        let (cfg, _om, devo, base) = setup();
        let mut entries = base.clone();
        let members: Vec<SigningKey> = (10u8..20).map(key).collect();
        for (i, k) in members.iter().enumerate() {
            entries.push(sign(
                cfg.network_id,
                &devo,
                Op::Add { node_id: id(k), hostname: "m".into(), ts: 2 + i as u64 },
            ));
        }
        let r = Roster::build(&cfg, &entries);

        // Every member gets a distinct host in the /24 (2..=254).
        let ips: Vec<Ipv4Addr> = r.members().map(|(_, m)| m.virtual_ip).collect();
        let uniq: std::collections::BTreeSet<_> = ips.iter().collect();
        assert_eq!(ips.len(), uniq.len(), "all member IPs must be distinct");
        for a in &ips {
            assert_eq!(&a.octets()[..3], &[10, 99, 0], "in subnet");
            assert!((2..=254).contains(&a.octets()[3]), "valid host");
        }

        // Entry order must not affect the assignment (every node agrees).
        let mut shuffled = base;
        for (i, k) in members.iter().enumerate().rev() {
            shuffled.push(sign(
                cfg.network_id,
                &devo,
                Op::Add { node_id: id(k), hostname: "m".into(), ts: 2 + i as u64 },
            ));
        }
        let r2 = Roster::build(&cfg, &shuffled);
        for (idk, m) in r.members() {
            assert_eq!(m.virtual_ip, r2.member(idk).unwrap().virtual_ip, "deterministic");
        }
    }

    #[test]
    fn concurrent_adds_get_distinct_ips() {
        // The race fix: two members approving two different joiners at the SAME
        // timestamp still yield distinct IPs (the approver no longer picks the IP
        // — it's derived from the NodeId during the fold).
        let (cfg, _om, devo, mut entries) = setup();
        let laptop = key(3);
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add { node_id: id(&laptop), hostname: "laptop".into(), ts: 2 },
        ));
        let a = key(40);
        let b = key(41);
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add { node_id: id(&a), hostname: "a".into(), ts: 5 },
        ));
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Add { node_id: id(&b), hostname: "b".into(), ts: 5 },
        ));
        let r = Roster::build(&cfg, &entries);
        assert_ne!(
            r.member(&id(&a)).unwrap().virtual_ip,
            r.member(&id(&b)).unwrap().virtual_ip,
            "concurrently-approved members must not share an IP"
        );
    }

    #[test]
    fn far_future_timestamps_are_dropped() {
        let (cfg, om, devo, _base) = setup();
        // An otherwise-valid genesis add, but dated far beyond the skew ceiling.
        let future = sign(
            cfg.network_id,
            &om,
            Op::Add {
                node_id: id(&devo),
                hostname: "x".into(),
                ts: now_ms() + 48 * 60 * 60 * 1000, // +48h
            },
        );
        let r = Roster::build(&cfg, std::slice::from_ref(&future));
        assert!(r.is_empty(), "far-future entry must be dropped");
    }

    #[test]
    fn member_cannot_vouch_before_it_joined() {
        let (cfg, _om, devo, mut entries) = setup(); // devo admitted at ts=1
        let laptop = key(3);
        entries.push(sign(
            cfg.network_id,
            &devo,
            Op::Add { node_id: id(&laptop), hostname: "l".into(), ts: 5 }, // laptop joins at 5
        ));
        let early = key(8);
        let late = key(9);
        // laptop vouches `early` with a ts BEFORE its own admission -> rejected.
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Add { node_id: id(&early), hostname: "e".into(), ts: 3 },
        ));
        // ...and `late` with a ts after its admission -> accepted.
        entries.push(sign(
            cfg.network_id,
            &laptop,
            Op::Add { node_id: id(&late), hostname: "L".into(), ts: 6 },
        ));
        let r = Roster::build(&cfg, &entries);
        assert!(!r.is_member(&id(&early)), "backdated vouch (before voucher joined) rejected");
        assert!(r.is_member(&id(&late)), "valid vouch accepted");
    }

    #[test]
    fn set_name_last_writer_wins_and_requires_membership() {
        let (cfg, _om, devo, mut entries) = setup(); // devo is a genesis member
        let outsider = key(50);
        entries.push(sign(cfg.network_id, &devo, Op::SetName { name: "Home".into(), ts: 10 }));
        entries.push(sign(cfg.network_id, &devo, Op::SetName { name: "Lab".into(), ts: 20 }));
        // A non-member's rename (even though it's the latest) is ignored.
        entries.push(sign(
            cfg.network_id,
            &outsider,
            Op::SetName { name: "Hacked".into(), ts: 30 },
        ));
        let r = Roster::build(&cfg, &entries);
        assert_eq!(r.name(), Some("Lab"));
    }
}
