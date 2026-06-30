//! The data-plane routing primitives: a forwarding table (virtual IP → member
//! NodeId) derived from the roster, minimal IPv4 header parsing, and the
//! cross-platform [`TunDevice`] abstraction.
//!
//! The actual pump (TUN read → lookup dst → send over that peer's iroh datagram;
//! inbound datagram → TUN write) lives in the engine, which owns the live
//! NodeId→Connection map. These pieces are kept pure so they're unit-testable
//! without a real network interface (which needs elevated privileges).

use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;

use crate::roster::{Id, Roster};

/// Virtual IP → member NodeId forwarding table, rebuilt whenever the roster
/// changes.
#[derive(Default, Clone, Debug)]
pub struct RouteTable {
    by_ip: HashMap<Ipv4Addr, Id>,
}

impl RouteTable {
    /// Build the table from the current roster's IP assignments.
    pub fn from_roster(roster: &Roster) -> Self {
        let mut by_ip = HashMap::new();
        for (id, member) in roster.members() {
            by_ip.insert(member.virtual_ip, *id);
        }
        Self { by_ip }
    }

    /// Which member owns `ip`, if any.
    pub fn lookup(&self, ip: &Ipv4Addr) -> Option<Id> {
        self.by_ip.get(ip).copied()
    }

    pub fn len(&self) -> usize {
        self.by_ip.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ip.is_empty()
    }
}

/// Destination IPv4 address of a raw IP packet, or `None` if it isn't IPv4 or is
/// too short. (We only route IPv4 in the virtual /24; IPv6 packets are dropped.)
pub fn dst_ipv4(pkt: &[u8]) -> Option<Ipv4Addr> {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 {
        return None;
    }
    Some(Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]))
}

/// Source IPv4 address of a raw IP packet (used to sanity-check inbound packets).
pub fn src_ipv4(pkt: &[u8]) -> Option<Ipv4Addr> {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 {
        return None;
    }
    Some(Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]))
}

/// A connection 5-tuple, used by the conntrack one-way "disable remote access"
/// block. Ports are 0 for non-TCP/UDP protocols (matched coarsely by address).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FlowKey {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub src_port: u16,
    pub dst_port: u16,
}

impl FlowKey {
    /// The key the matching reverse-direction flow would have (return traffic).
    pub fn reversed(&self) -> FlowKey {
        FlowKey {
            src: self.dst,
            dst: self.src,
            proto: self.proto,
            src_port: self.dst_port,
            dst_port: self.src_port,
        }
    }
}

/// Parse an IPv4 packet's 5-tuple, or `None` if it isn't IPv4 / is too short.
pub fn flow_key(pkt: &[u8]) -> Option<FlowKey> {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 {
        return None;
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    if ihl < 20 || pkt.len() < ihl {
        return None;
    }
    let proto = pkt[9];
    let src = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
    let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
    // TCP (6) and UDP (17) carry ports in the first 4 bytes of their header.
    let (src_port, dst_port) = match proto {
        6 | 17 if pkt.len() >= ihl + 4 => (
            u16::from_be_bytes([pkt[ihl], pkt[ihl + 1]]),
            u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]),
        ),
        _ => (0, 0),
    };
    Some(FlowKey {
        src,
        dst,
        proto,
        src_port,
        dst_port,
    })
}

/// TCP **MSS clamping**: if `pkt` is an IPv4 TCP SYN whose MSS option exceeds
/// `max_mss`, lower it to `max_mss` and fix the TCP checksum. Returns whether it
/// changed anything.
///
/// This is the standard tunnel fix for the "packet too big" problem: by capping
/// the Maximum Segment Size at connection setup, TCP flows (RDP/SSH/file copy)
/// never produce segments larger than the tunnel's datagram limit, so we don't
/// silently drop full-size packets and stall. Applied to SYNs in both directions.
pub fn clamp_tcp_mss(pkt: &mut [u8], max_mss: u16) -> bool {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 {
        return false;
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    if ihl < 20 || pkt.len() < ihl + 20 || pkt[9] != 6 {
        return false; // too short or not TCP
    }
    let tcp = ihl;
    let data_off = ((pkt[tcp + 12] >> 4) as usize) * 4;
    if data_off < 20 || pkt.len() < tcp + data_off {
        return false;
    }
    if pkt[tcp + 13] & 0x02 == 0 {
        return false; // not a SYN
    }
    // Walk TCP options for kind 2 (MSS), length 4.
    let mut i = tcp + 20;
    let end = tcp + data_off;
    let mut changed = false;
    while i + 1 < end {
        match pkt[i] {
            0 => break,         // end of option list
            1 => i += 1,        // NOP
            kind => {
                let len = pkt[i + 1] as usize;
                if len < 2 || i + len > end {
                    break;
                }
                if kind == 2 && len == 4 {
                    let mss = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]);
                    if mss > max_mss {
                        pkt[i + 2..i + 4].copy_from_slice(&max_mss.to_be_bytes());
                        changed = true;
                    }
                }
                i += len;
            }
        }
    }
    if changed {
        recompute_tcp_checksum(pkt, ihl);
    }
    changed
}

/// Recompute the TCP checksum over the pseudo-header + segment.
fn recompute_tcp_checksum(pkt: &mut [u8], ihl: usize) {
    let tcp = ihl;
    let tcp_len = pkt.len() - tcp;
    pkt[tcp + 16] = 0;
    pkt[tcp + 17] = 0;
    let mut sum: u32 = 0;
    // Pseudo-header: src + dst IPs, zero, protocol(6), TCP length.
    for chunk in [&pkt[12..14], &pkt[14..16], &pkt[16..18], &pkt[18..20]] {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    sum += 6;
    sum += tcp_len as u32;
    // TCP header + payload.
    let mut i = tcp;
    while i + 1 < pkt.len() {
        sum += u16::from_be_bytes([pkt[i], pkt[i + 1]]) as u32;
        i += 2;
    }
    if i < pkt.len() {
        sum += (pkt[i] as u32) << 8; // last odd byte
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let check = !(sum as u16);
    pkt[tcp + 16..tcp + 18].copy_from_slice(&check.to_be_bytes());
}

/// A cross-platform TUN interface. Implemented over `tun-rs` on desktop and over
/// the `VpnService` fd on Android; a channel-backed mock is used in tests.
///
/// Uses `async fn in trait` (RPITIT), so it's consumed generically rather than as
/// a `dyn` object — each platform provides one concrete type.
pub trait TunDevice: Send + Sync + 'static {
    /// Read one IP packet from the OS into `buf`, returning its length.
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = std::io::Result<usize>> + Send;
    /// Write one IP packet to the OS.
    fn send(&self, pkt: &[u8]) -> impl Future<Output = std::io::Result<()>> + Send;
    /// The interface MTU (clamped below the iroh datagram limit).
    fn mtu(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{sign, Config, Op};
    use ed25519_dalek::SigningKey;

    fn ipv4_packet(src: Ipv4Addr, dst: Ipv4Addr) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[0] = 0x45; // version 4, IHL 5
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p
    }

    #[test]
    fn parses_ipv4_addresses() {
        let p = ipv4_packet(Ipv4Addr::new(10, 99, 0, 2), Ipv4Addr::new(10, 99, 0, 7));
        assert_eq!(dst_ipv4(&p), Some(Ipv4Addr::new(10, 99, 0, 7)));
        assert_eq!(src_ipv4(&p), Some(Ipv4Addr::new(10, 99, 0, 2)));
    }

    #[test]
    fn rejects_non_ipv4_and_short() {
        let mut p = ipv4_packet(Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED);
        p[0] = 0x60; // IPv6
        assert_eq!(dst_ipv4(&p), None);
        assert_eq!(dst_ipv4(&[0u8; 4]), None);
    }

    #[test]
    fn route_table_maps_members_to_node_ids() {
        let om = SigningKey::from_bytes(&[1u8; 32]);
        let devo = SigningKey::from_bytes(&[2u8; 32]);
        let net = [9u8; 32];
        let cfg = Config {
            network_id: net,
            originator_id: om.verifying_key().to_bytes(),
            subnet: Ipv4Addr::new(10, 99, 0, 0),
        };
        let devo_id = devo.verifying_key().to_bytes();
        let entries = vec![sign(
            net,
            &om,
            Op::Add {
                node_id: devo_id,
                hostname: "o".into(),
                role: crate::roster::Role::Controller,
                virtual_ip: [10, 99, 0, 2],
                invite_nonce: [0u8; 16],
                ts: 1,
            },
        )];
        let roster = Roster::build(&cfg, &entries);
        // The member's IP is assigned by the roster; the table maps it to the node.
        let ip = roster.member(&devo_id).unwrap().virtual_ip;
        let table = RouteTable::from_roster(&roster);
        assert_eq!(table.lookup(&ip), Some(devo_id));
        assert_eq!(table.lookup(&Ipv4Addr::new(10, 99, 0, 200)), None);
    }

    /// Build an IPv4 TCP SYN with a single MSS option.
    fn tcp_syn_with_mss(mss: u16) -> Vec<u8> {
        let mut p = vec![0u8; 44]; // 20 IPv4 + 24 TCP (20 + 4-byte MSS option)
        p[0] = 0x45; // v4, IHL 5
        p[9] = 6; // TCP
        p[12..16].copy_from_slice(&Ipv4Addr::new(10, 99, 0, 2).octets());
        p[16..20].copy_from_slice(&Ipv4Addr::new(10, 99, 0, 7).octets());
        let tcp = 20;
        p[tcp + 12] = 6 << 4; // data offset = 6 words (24 bytes)
        p[tcp + 13] = 0x02; // SYN
        p[tcp + 20] = 2; // MSS option kind
        p[tcp + 21] = 4; // length
        p[tcp + 22..tcp + 24].copy_from_slice(&mss.to_be_bytes());
        p
    }

    /// One's-complement sum over pseudo-header + TCP segment; valid == 0xFFFF.
    fn tcp_checksum_ok(pkt: &[u8]) -> bool {
        let tcp = 20;
        let mut sum: u32 = 0;
        for c in [&pkt[12..14], &pkt[14..16], &pkt[16..18], &pkt[18..20]] {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        sum += 6 + (pkt.len() - tcp) as u32;
        let mut i = tcp;
        while i + 1 < pkt.len() {
            sum += u16::from_be_bytes([pkt[i], pkt[i + 1]]) as u32;
            i += 2;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        sum as u16 == 0xffff
    }

    #[test]
    fn mss_clamp_lowers_and_fixes_checksum() {
        let mut p = tcp_syn_with_mss(1460);
        assert!(clamp_tcp_mss(&mut p, 1240), "oversized MSS should be clamped");
        assert_eq!(u16::from_be_bytes([p[42], p[43]]), 1240, "MSS lowered to clamp");
        assert!(tcp_checksum_ok(&p), "TCP checksum valid after clamp");
    }

    #[test]
    fn mss_clamp_leaves_small_mss_and_non_syn() {
        // Already-small MSS: unchanged.
        let mut small = tcp_syn_with_mss(1000);
        assert!(!clamp_tcp_mss(&mut small, 1240));
        assert_eq!(u16::from_be_bytes([small[42], small[43]]), 1000);
        // Non-SYN: untouched even with a big MSS field.
        let mut not_syn = tcp_syn_with_mss(1460);
        not_syn[20 + 13] = 0x10; // ACK only, no SYN
        assert!(!clamp_tcp_mss(&mut not_syn, 1240));
        assert_eq!(u16::from_be_bytes([not_syn[42], not_syn[43]]), 1460);
    }
}
