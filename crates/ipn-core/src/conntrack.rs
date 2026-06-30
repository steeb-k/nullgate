//! Stateful connection tracking for the one-way **"Disable remote access"**
//! block.
//!
//! When a device turns the switch on, it should still be able to reach other
//! members (RDP/SSH *out*), but no member should be able to initiate to it
//! (*in*). A stateless "drop all inbound" filter can't do that — it would also
//! drop the return traffic of connections this device started. So we track the
//! flows we initiate (on the outbound TUN→mesh path) and, while the block is on,
//! admit an inbound packet only if it matches the reverse of a tracked flow.
//!
//! The table is keyed by [`FlowKey`] and stores a coarse last-seen timestamp;
//! entries idle past [`FLOW_TTL_MS`] are swept on the periodic engine tick. It
//! lives behind a plain `RwLock` (the same lock discipline as the route/conn
//! tables) so the per-packet pump never touches the async state mutex.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::router::{flow_key, FlowKey};

/// Idle lifetime of a tracked flow. Long enough to cover a quiet RDP/SSH session
/// between keepalives; short enough that the table self-trims.
const FLOW_TTL_MS: u64 = 120_000;

/// Tracks the flows this device initiated, so return traffic is allowed back in
/// while unsolicited inbound is dropped.
#[derive(Default)]
pub struct Conntrack {
    flows: RwLock<HashMap<FlowKey, u64>>,
}

impl Conntrack {
    /// Record a flow we initiated (called on every outbound TUN→mesh packet so an
    /// already-established connection keeps working if the block is toggled on
    /// mid-session). `now` is the engine's coarse clock (ms).
    pub fn record_outbound(&self, pkt: &[u8], now: u64) {
        if let Some(k) = flow_key(pkt) {
            self.flows.write().unwrap().insert(k, now);
        }
    }

    /// Whether an inbound packet is return traffic for a flow we initiated.
    pub fn allows_inbound(&self, pkt: &[u8], now: u64) -> bool {
        let Some(k) = flow_key(pkt) else {
            return false;
        };
        match self.flows.read().unwrap().get(&k.reversed()) {
            Some(&seen) => now.saturating_sub(seen) <= FLOW_TTL_MS,
            None => false,
        }
    }

    /// Drop flows idle past the TTL (called from the periodic tick).
    pub fn sweep(&self, now: u64) {
        self.flows
            .write()
            .unwrap()
            .retain(|_, &mut seen| now.saturating_sub(seen) <= FLOW_TTL_MS);
    }

    /// Forget all tracked flows (on disconnect / teardown).
    pub fn clear(&self) {
        self.flows.write().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Minimal IPv4 TCP packet with the given addresses/ports.
    fn tcp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16) -> Vec<u8> {
        let mut p = vec![0u8; 24];
        p[0] = 0x45; // v4, IHL 5
        p[9] = 6; // TCP
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn return_traffic_allowed_unsolicited_dropped() {
        let me = Ipv4Addr::new(10, 99, 0, 2);
        let peer = Ipv4Addr::new(10, 99, 0, 5);
        let ct = Conntrack::default();

        // We initiate me:51000 -> peer:22.
        let out = tcp(me, peer, 51000, 22);
        ct.record_outbound(&out, 1000);

        // Peer's reply (peer:22 -> me:51000) is allowed.
        let reply = tcp(peer, me, 22, 51000);
        assert!(ct.allows_inbound(&reply, 1001));

        // An unsolicited inbound (peer:40000 -> me:3389) is dropped.
        let unsolicited = tcp(peer, me, 40000, 3389);
        assert!(!ct.allows_inbound(&unsolicited, 1001));
    }

    #[test]
    fn expired_flow_is_not_matched() {
        let me = Ipv4Addr::new(10, 99, 0, 2);
        let peer = Ipv4Addr::new(10, 99, 0, 5);
        let ct = Conntrack::default();
        ct.record_outbound(&tcp(me, peer, 51000, 22), 1000);
        let reply = tcp(peer, me, 22, 51000);
        // Past the TTL the reply no longer matches.
        assert!(!ct.allows_inbound(&reply, 1000 + FLOW_TTL_MS + 1));
    }
}
