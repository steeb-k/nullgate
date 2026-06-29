//! Live peer presence — the data behind the member list's green dot, hostname,
//! observed public IP, and "last seen".
//!
//! Each member periodically broadcasts a **signed** [`Presence`] over iroh-gossip
//! on the network's private topic (never the public DHT), so an outsider can't
//! forge it and a non-member can't even read it. The *hostname* is self-reported
//! (signed). The *public IP* is deliberately NOT taken from the presence message
//! — it's filled in from what this node actually observes for the peer's
//! connection ([`crate::node`] → `remote_info`), so a member can't spoof its own
//! address. This module owns the signed message + a [`PresenceTracker`] that the
//! engine updates; the gossip plumbing lives in the engine.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::roster::Id;

const DOMAIN: &str = "ipn-presence-v1";

/// A signed presence heartbeat broadcast by a member. Carries the device's
/// **actual current** OS hostname (the shared identifier) and its self-known
/// **public IP** (advertised so peers can show it even over a relay path).
/// Friendly names are a per-client local nickname and are never broadcast.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Presence {
    pub network_id: Id,
    pub node_id: Id,
    pub hostname: String,
    /// The member's own public/internet-facing IP, as it knows it (advertised).
    pub public_ip: Option<String>,
    /// Milliseconds since the Unix epoch when this heartbeat was produced.
    pub ts: u64,
    pub signature: Vec<u8>,
}

impl Presence {
    /// Create and sign a presence heartbeat with this device's key.
    pub fn signed(
        network_id: Id,
        key: &SigningKey,
        hostname: String,
        public_ip: Option<String>,
        ts: u64,
    ) -> Self {
        let node_id = key.verifying_key().to_bytes();
        let sig = key.sign(&signing_bytes(&network_id, &node_id, &hostname, &public_ip, ts));
        Self {
            network_id,
            node_id,
            hostname,
            public_ip,
            ts,
            signature: sig.to_bytes().to_vec(),
        }
    }

    /// Verify the heartbeat was signed by `node_id` for this network.
    pub fn verify(&self, network_id: &Id) -> bool {
        if &self.network_id != network_id {
            return false;
        }
        let Ok(sig_arr): Result<[u8; 64], _> = self.signature.as_slice().try_into() else {
            return false;
        };
        let sig = Signature::from_bytes(&sig_arr);
        let Ok(vk) = VerifyingKey::from_bytes(&self.node_id) else {
            return false;
        };
        vk.verify_strict(
            &signing_bytes(
                &self.network_id,
                &self.node_id,
                &self.hostname,
                &self.public_ip,
                self.ts,
            ),
            &sig,
        )
        .is_ok()
    }
}

fn signing_bytes(
    network_id: &Id,
    node_id: &Id,
    hostname: &str,
    public_ip: &Option<String>,
    ts: u64,
) -> Vec<u8> {
    #[derive(Serialize)]
    struct View<'a> {
        domain: &'static str,
        network_id: &'a Id,
        node_id: &'a Id,
        hostname: &'a str,
        public_ip: &'a Option<String>,
        ts: u64,
    }
    let mut buf = Vec::new();
    ciborium::into_writer(
        &View {
            domain: DOMAIN,
            network_id,
            node_id,
            hostname,
            public_ip,
            ts,
        },
        &mut buf,
    )
    .expect("serialize presence view");
    buf
}

/// Originator-asserted geolocation for members, signed with the **originator
/// master key** so peers can trust it. Maps each member's NodeId to a
/// `"City, Country"` string the originator resolved from its advertised public IP.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Locations {
    pub network_id: Id,
    pub ts: u64,
    pub entries: Vec<(Id, String)>,
    pub signature: Vec<u8>,
}

impl Locations {
    pub fn signed(network_id: Id, originator_key: &SigningKey, entries: Vec<(Id, String)>, ts: u64) -> Self {
        let sig = originator_key.sign(&loc_signing_bytes(&network_id, &entries, ts));
        Self {
            network_id,
            ts,
            entries,
            signature: sig.to_bytes().to_vec(),
        }
    }

    /// Verify the message was signed by this network's originator master key.
    pub fn verify(&self, network_id: &Id, originator_id: &Id) -> bool {
        if &self.network_id != network_id {
            return false;
        }
        let Ok(sig): Result<[u8; 64], _> = self.signature.as_slice().try_into() else {
            return false;
        };
        let Ok(vk) = VerifyingKey::from_bytes(originator_id) else {
            return false;
        };
        vk.verify_strict(
            &loc_signing_bytes(&self.network_id, &self.entries, self.ts),
            &Signature::from_bytes(&sig),
        )
        .is_ok()
    }
}

fn loc_signing_bytes(network_id: &Id, entries: &[(Id, String)], ts: u64) -> Vec<u8> {
    #[derive(Serialize)]
    struct View<'a> {
        domain: &'static str,
        network_id: &'a Id,
        entries: &'a [(Id, String)],
        ts: u64,
    }
    let mut buf = Vec::new();
    ciborium::into_writer(
        &View {
            domain: "ipn-locations-v1",
            network_id,
            entries,
            ts,
        },
        &mut buf,
    )
    .expect("serialize locations view");
    buf
}

/// Envelope for everything sent on the presence gossip topic.
#[derive(Serialize, Deserialize)]
pub enum GossipMsg {
    Presence(Presence),
    Locations(Locations),
}

/// What the UI shows for one peer.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PeerStatus {
    pub hostname: Option<String>,
    /// "City, Country" resolved by the originator (propagated, not self-reported).
    pub location: Option<String>,
    pub virtual_ip: Option<Ipv4Addr>,
    /// Peer's private/LAN address (IP only), if iroh knows one.
    pub local_ip: Option<String>,
    /// Peer's public/internet-facing address (IP only), if iroh knows one.
    pub public_ip: Option<String>,
    /// Peer-observed active address (with port / relay), un-spoofable.
    pub observed_addr: Option<String>,
    /// Whether the path is direct (true) or via relay (false); `None` if unknown.
    pub direct: Option<bool>,
    /// Milliseconds since epoch of the last signed heartbeat we accepted.
    pub last_seen: u64,
    /// Whether we currently hold a live connection to this peer.
    pub online: bool,
}

/// Tracks the latest presence/observation for each known peer. The engine feeds
/// it heartbeats (from gossip) and connection observations (from iroh).
#[derive(Default)]
pub struct PresenceTracker {
    peers: HashMap<Id, PeerStatus>,
}

impl PresenceTracker {
    /// Record a verified heartbeat (monotonic: older timestamps are ignored).
    pub fn record_heartbeat(
        &mut self,
        node_id: Id,
        hostname: String,
        public_ip: Option<String>,
        ts: u64,
    ) {
        let e = self.peers.entry(node_id).or_default();
        if ts >= e.last_seen {
            e.last_seen = ts;
            e.hostname = Some(hostname);
            // The peer advertises its own public IP; prefer it when present.
            if public_ip.is_some() {
                e.public_ip = public_ip;
            }
        }
    }

    /// Seed a peer's last-seen time (from the persisted store at startup) so the
    /// "offline > 1 week" indicator survives daemon restarts.
    pub fn set_last_seen(&mut self, node_id: Id, ts: u64) {
        let e = self.peers.entry(node_id).or_default();
        if ts > e.last_seen {
            e.last_seen = ts;
        }
    }

    /// Record what we observe about a live connection to a peer.
    pub fn record_connection(
        &mut self,
        node_id: Id,
        observed_addr: Option<String>,
        direct: Option<bool>,
        local_ip: Option<String>,
        public_ip: Option<String>,
        online: bool,
    ) {
        let e = self.peers.entry(node_id).or_default();
        e.online = online;
        e.direct = direct;
        if observed_addr.is_some() {
            e.observed_addr = observed_addr;
        }
        if local_ip.is_some() {
            e.local_ip = local_ip;
        }
        if public_ip.is_some() {
            e.public_ip = public_ip;
        }
    }

    /// Set the roster-assigned virtual IP for a peer.
    pub fn set_virtual_ip(&mut self, node_id: Id, ip: Ipv4Addr) {
        self.peers.entry(node_id).or_default().virtual_ip = Some(ip);
    }

    /// Set a peer's resolved location (from the originator's propagated map).
    pub fn set_location(&mut self, node_id: Id, location: Option<String>) {
        self.peers.entry(node_id).or_default().location = location;
    }

    pub fn get(&self, node_id: &Id) -> Option<&PeerStatus> {
        self.peers.get(node_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Id, &PeerStatus)> {
        self.peers.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn presence_signs_and_verifies() {
        let net = [9u8; 32];
        let k = key(5);
        let p = Presence::signed(net, &k, "laptop".into(), Some("1.2.3.4".into()), 1000);
        assert!(p.verify(&net));
        // Wrong network rejected.
        assert!(!p.verify(&[8u8; 32]));
    }

    #[test]
    fn tampered_presence_fails() {
        let net = [9u8; 32];
        let mut p = Presence::signed(net, &key(5), "laptop".into(), None, 1000);
        p.hostname = "imposter".into();
        assert!(!p.verify(&net));
        // The advertised public IP is signed too.
        let mut q = Presence::signed(net, &key(5), "laptop".into(), Some("1.2.3.4".into()), 1000);
        q.public_ip = Some("9.9.9.9".into());
        assert!(!q.verify(&net));
    }

    #[test]
    fn forged_node_id_fails() {
        let net = [9u8; 32];
        let mut p = Presence::signed(net, &key(5), "laptop".into(), None, 1000);
        p.node_id = key(6).verifying_key().to_bytes(); // claim someone else
        assert!(!p.verify(&net));
    }

    #[test]
    fn tracker_keeps_latest_heartbeat() {
        let mut t = PresenceTracker::default();
        let id = key(5).verifying_key().to_bytes();
        t.record_heartbeat(id, "old".into(), None, 100);
        t.record_heartbeat(id, "new".into(), Some("1.2.3.4".into()), 200);
        t.record_heartbeat(id, "stale".into(), None, 150); // ignored
        assert_eq!(t.get(&id).unwrap().hostname.as_deref(), Some("new"));
        assert_eq!(t.get(&id).unwrap().public_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(t.get(&id).unwrap().last_seen, 200);
    }
}
