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

/// A signed presence heartbeat broadcast by a member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Presence {
    pub network_id: Id,
    pub node_id: Id,
    /// The device's **actual current** OS hostname (re-read each beat; the source
    /// of truth, not member-editable).
    pub hostname: String,
    /// Optional friendly name the member chose for itself.
    pub label: Option<String>,
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
        label: Option<String>,
        ts: u64,
    ) -> Self {
        let node_id = key.verifying_key().to_bytes();
        let sig = key.sign(&signing_bytes(&network_id, &node_id, &hostname, &label, ts));
        Self {
            network_id,
            node_id,
            hostname,
            label,
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
            &signing_bytes(&self.network_id, &self.node_id, &self.hostname, &self.label, self.ts),
            &sig,
        )
        .is_ok()
    }
}

fn signing_bytes(
    network_id: &Id,
    node_id: &Id,
    hostname: &str,
    label: &Option<String>,
    ts: u64,
) -> Vec<u8> {
    #[derive(Serialize)]
    struct View<'a> {
        domain: &'static str,
        network_id: &'a Id,
        node_id: &'a Id,
        hostname: &'a str,
        label: &'a Option<String>,
        ts: u64,
    }
    let mut buf = Vec::new();
    ciborium::into_writer(
        &View {
            domain: DOMAIN,
            network_id,
            node_id,
            hostname,
            label,
            ts,
        },
        &mut buf,
    )
    .expect("serialize presence view");
    buf
}

/// What the UI shows for one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerStatus {
    pub hostname: Option<String>,
    pub label: Option<String>,
    pub virtual_ip: Option<Ipv4Addr>,
    /// Peer-observed public address (from our connection to them), un-spoofable.
    pub observed_addr: Option<String>,
    /// Whether the path is direct (true) or via relay (false); `None` if unknown.
    pub direct: Option<bool>,
    /// Milliseconds since epoch of the last signed heartbeat we accepted.
    pub last_seen: u64,
    /// Whether we currently hold a live connection to this peer.
    pub online: bool,
}

impl Default for PeerStatus {
    fn default() -> Self {
        Self {
            hostname: None,
            label: None,
            virtual_ip: None,
            observed_addr: None,
            direct: None,
            last_seen: 0,
            online: false,
        }
    }
}

/// Tracks the latest presence/observation for each known peer. The engine feeds
/// it heartbeats (from gossip) and connection observations (from iroh).
#[derive(Default)]
pub struct PresenceTracker {
    peers: HashMap<Id, PeerStatus>,
}

impl PresenceTracker {
    /// Record a verified heartbeat (monotonic: older timestamps are ignored).
    pub fn record_heartbeat(&mut self, node_id: Id, hostname: String, label: Option<String>, ts: u64) {
        let e = self.peers.entry(node_id).or_default();
        if ts >= e.last_seen {
            e.last_seen = ts;
            e.hostname = Some(hostname);
            e.label = label;
        }
    }

    /// Record what we observe about a live connection to a peer.
    pub fn record_connection(&mut self, node_id: Id, observed_addr: Option<String>, direct: Option<bool>, online: bool) {
        let e = self.peers.entry(node_id).or_default();
        e.online = online;
        e.direct = direct;
        if observed_addr.is_some() {
            e.observed_addr = observed_addr;
        }
    }

    /// Set the roster-assigned virtual IP for a peer.
    pub fn set_virtual_ip(&mut self, node_id: Id, ip: Ipv4Addr) {
        self.peers.entry(node_id).or_default().virtual_ip = Some(ip);
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
        let p = Presence::signed(net, &k, "laptop".into(), Some("My Laptop".into()), 1000);
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
        // Tampering with the label is also caught (it's signed).
        let mut q = Presence::signed(net, &key(5), "laptop".into(), Some("real".into()), 1000);
        q.label = Some("fake".into());
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
        t.record_heartbeat(id, "new".into(), Some("New".into()), 200);
        t.record_heartbeat(id, "stale".into(), None, 150); // ignored
        assert_eq!(t.get(&id).unwrap().hostname.as_deref(), Some("new"));
        assert_eq!(t.get(&id).unwrap().label.as_deref(), Some("New"));
        assert_eq!(t.get(&id).unwrap().last_seen, 200);
    }
}
