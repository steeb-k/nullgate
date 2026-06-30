//! The engine: ties the iroh node, the signed roster (over iroh-docs), the
//! authenticated member mesh, and gossip presence into one object the UI drives.
//!
//! v1 scope: create / join (with emoji SAS approval), the member-to-member mesh
//! (PSK proof + roster gate), and live presence. The TUN packet pump layers on
//! top of the same `mesh` connection map next.
//!
//! Concurrency: all mutable state lives behind one async `Mutex`; network I/O is
//! always done off-lock (snapshot under lock → I/O → re-lock to store). Custom
//! ALPNs (`mesh`, `join`) are accepted by the router and forwarded to engine
//! tasks over channels.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use ed25519_dalek::SigningKey;
use futures_lite::StreamExt;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use iroh_docs::api::Doc;
use iroh_docs::AuthorId;
use iroh_gossip::api::{Event, GossipSender};
use iroh_gossip::proto::TopicId;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::admission;
use crate::membership;
use crate::conntrack::Conntrack;
use crate::network::{
    decode_recovery_key, encode_recovery_key, generate_originator_key, NetworkSecret, Ticket,
};
use crate::node::IrohNode;
use crate::presence::{GossipMsg, Locations, Presence, PresenceTracker};
use crate::roster::{now_ms, sign, Config, Id, InviteKind, Nonce, Op, Role, Roster};
use crate::router::{clamp_tcp_mss, dst_ipv4, RouteTable};
use crate::tun_device::RealTun;

/// TUN MTU: clamped well under the QUIC datagram limit (~1200–1400B after
/// overhead) so one IP packet always fits one datagram. Inner TCP (RDP/SSH)
/// adapts via PMTU.
const TUN_MTU: u16 = 1280;

/// TCP MSS we clamp SYNs to (`MTU - IPv4(20) - TCP(20)`), so TCP flows never
/// exceed the tunnel and get black-holed.
const TUN_MSS: u16 = TUN_MTU - 40;

const MESH_ALPN: &[u8] = b"ipn/mesh/0";
const JOIN_ALPN: &[u8] = b"ipn/join/0";
const CONFIG_FILE: &str = "network.cbor";

// ---------------------------------------------------------------------------
// Persisted configuration
// ---------------------------------------------------------------------------

/// In-memory network config. The secret bytes live in the OS keystore (see
/// [`crate::secrets`]); only the non-secret fields are written to disk.
#[derive(Clone)]
struct StoredConfig {
    name: String,
    subnet: [u8; 4],
    secret: [u8; 32],
    originator_id: Id,
    /// Present only on the originator's device (the exportable master authority).
    originator_secret: Option<[u8; 32]>,
}

/// The non-secret part of the config persisted to `network.cbor`.
#[derive(Serialize, Deserialize)]
struct OnDiskConfig {
    name: String,
    subnet: [u8; 4],
    originator_id: Id,
}

/// Keystore key names for this device's network secrets (one network at a time).
const KEY_NETWORK_SECRET: &str = "network-secret";
const KEY_ORIGINATOR_SECRET: &str = "originator-secret";

impl StoredConfig {
    fn secret(&self) -> NetworkSecret {
        NetworkSecret::from_bytes(self.secret)
    }
    fn roster_cfg(&self) -> Config {
        Config {
            network_id: self.secret().network_id(),
            originator_id: self.originator_id,
            subnet: self.subnet(),
        }
    }
    fn subnet(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.subnet)
    }
}

// ---------------------------------------------------------------------------
// Public views consumed by the UI / CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemberView {
    pub node_id: String,
    /// The device's actual current OS hostname (the shared identifier).
    pub hostname: Option<String>,
    /// This client's **local** friendly nickname for the member (not shared).
    pub label: Option<String>,
    pub virtual_ip: Option<String>,
    /// Peer's private/LAN IP (no port), if known.
    pub local_ip: Option<String>,
    /// Peer's public/internet-facing IP (no port), if known.
    pub public_ip: Option<String>,
    /// "City, Country" resolved by the originator from the public IP, if available.
    pub location: Option<String>,
    pub observed_addr: Option<String>,
    pub direct: Option<bool>,
    pub online: bool,
    pub last_seen: u64,
    pub is_self: bool,
    pub is_originator_device: bool,
    /// This member's tier: `"peer"`, `"controller"`, or `"originator"`.
    #[serde(default)]
    pub role: String,
    /// Member has disabled inbound remote access (others can't reach it).
    #[serde(default)]
    pub access_disabled: bool,
    /// Member asked to be hidden (only surfaced to originators).
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub name: String,
    pub subnet: String,
    pub frozen: bool,
    pub self_node_id: String,
    pub self_ip: Option<String>,
    pub is_originator: bool,
    /// This device's effective tier: `"peer"`, `"controller"`, or `"originator"`.
    #[serde(default)]
    pub self_role: String,
    /// Whether the current Peer join ticket is single-use (drives the toggle).
    #[serde(default)]
    pub peer_ticket_single_use: bool,
    /// Whether the TUN is up so RDP/SSH traffic is actually routed (needs elevation).
    pub routing: bool,
    /// Whether the daemon is currently connected to the network (vs. disconnected
    /// via "Quit", but still holding the config).
    pub online: bool,
    /// This device's home relay URL, if one is established (diagnostics).
    pub home_relay: Option<String>,
    pub members: Vec<MemberView>,
}

/// One entry in the administration activity log — a human-readable view over a
/// signed roster operation (who did what, when). Derived, so it's tamper-evident
/// and the same for every member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Milliseconds since the Unix epoch (member-chosen; see roster ordering note).
    pub ts: u64,
    /// The signer of the operation (hex), or `"originator"` for master-key actions.
    pub actor_node_id: String,
    /// A friendly name for the actor, if resolvable.
    pub actor_name: Option<String>,
    /// Human-readable description of the action.
    pub action: String,
}

/// Events pushed to subscribers (the GUI) as things change.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// Something about the network/members changed; re-query [`Engine::status`].
    Changed,
    /// We (the joiner) computed the SAS for a join in progress — show it so the
    /// user can compare it with the approving member's screen.
    JoinSas { sas: Vec<String> },
    /// A device wants to join; an existing member should compare `sas` and approve.
    JoinRequest {
        node_id: String,
        hostname: String,
        sas: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Join wire messages
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct JoinRequest {
    hostname: String,
    /// Which tier the joiner's ticket authorizes (Peer or Controller).
    #[serde(default = "default_join_kind")]
    invite_kind: InviteKind,
    /// The invite nonce from the joiner's ticket; the admitting `Add` cites it.
    #[serde(default)]
    invite_nonce: Nonce,
}

fn default_join_kind() -> InviteKind {
    InviteKind::Peer
}

#[derive(Serialize, Deserialize)]
enum JoinResponse {
    Approved,
    Denied,
}

// ---------------------------------------------------------------------------
// Router protocol handler: forward accepted connections to the engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ChannelProto {
    tx: mpsc::Sender<Connection>,
}

impl ProtocolHandler for ChannelProto {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        // Ownership of the connection moves into the engine via the channel, so
        // it stays alive after this handler returns.
        let _ = self.tx.send(conn).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

struct PendingJoin {
    responder: oneshot::Sender<bool>,
}

struct State {
    config: Option<StoredConfig>,
    doc: Option<Doc>,
    author: Option<AuthorId>,
    roster: Roster,
    presence: PresenceTracker,
    pending: HashMap<Id, PendingJoin>,
    gossip_sender: Option<GossipSender>,
}

struct Inner {
    node: IrohNode,
    device_key: SigningKey,
    my_id: Id,
    /// This client's **local** friendly nicknames for other members, keyed by
    /// NodeId hex. Never broadcast — purely local display. The OS hostname (read
    /// live) is the shared identifier.
    nicknames: StdRwLock<HashMap<String, String>>,
    data_dir: PathBuf,
    state: Mutex<State>,
    events: broadcast::Sender<EngineEvent>,
    /// Live mesh connections by peer, kept in a cheap sync lock so the TUN read
    /// loop and per-connection datagram readers route without touching the async
    /// state mutex on every packet.
    conns: StdRwLock<HashMap<Id, Connection>>,
    /// Forwarding table (virtual IP → peer), rebuilt from the roster each tick.
    routes: StdRwLock<RouteTable>,
    /// The OS TUN interface, once routing is enabled (requires elevation).
    tun: StdRwLock<Option<Arc<RealTun>>>,
    /// Whether we've already attempted to bring up the TUN (open it once).
    tun_attempted: AtomicBool,
    /// This device blocks inbound remote access (one-way; outbound still works).
    /// Read lock-free on the per-packet inbound path, so it lives on `Inner`.
    remote_access_disabled: AtomicBool,
    /// This device asks to be hidden from the member list (implies the block).
    hidden: AtomicBool,
    /// Tracks flows we initiated, so the one-way block lets return traffic back in.
    conntrack: Conntrack,
    /// Coarse wall-clock (ms), refreshed each tick — read by the per-packet pump
    /// to stamp/age conntrack flows without a syscall per packet.
    coarse_now: AtomicU64,
    /// Abort handles for network-scoped background tasks (presence receiver, TUN
    /// read loop) so leaving/deleting a network stops them cleanly.
    net_tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Whether we've ever seen ourselves in the roster. Once true, dropping out of
    /// the roster means we were removed → auto-leave (handles remove/delete/rotate).
    was_member: AtomicBool,
    /// Our mesh/join protocol version (normally `admission::PROTOCOL_VERSION`;
    /// overridable in tests to exercise the mismatch path).
    protocol_version: AtomicU32,
    /// Last time (ms) we flushed the persisted last-seen map (throttle).
    last_seen_saved: AtomicU64,
    /// Geolocation DB, loaded only on the originator (it resolves + propagates).
    geo: StdRwLock<Option<crate::geo::GeoDb>>,
    /// Guards against launching multiple concurrent geo-DB downloads.
    geo_downloading: AtomicBool,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// Boot the node and start background loops. Loads + activates an existing
    /// network if one is stored in `data_dir`.
    pub async fn start(data_dir: impl AsRef<Path>) -> Result<Engine> {
        let data_dir = data_dir.as_ref().to_path_buf();

        let (mesh_tx, mesh_rx) = mpsc::channel::<Connection>(32);
        let (join_tx, join_rx) = mpsc::channel::<Connection>(32);
        let node = IrohNode::spawn_with(&data_dir, |b| {
            b.accept(MESH_ALPN, ChannelProto { tx: mesh_tx })
                .accept(JOIN_ALPN, ChannelProto { tx: join_tx })
        })
        .await?;

        let device_key = node.device_signing_key();
        let my_id = node.node_id_bytes();
        debug_assert_eq!(device_key.verifying_key().to_bytes(), my_id);
        let nicknames = load_nicknames(&data_dir);
        let prefs = load_device_prefs(&data_dir);

        let (events, _) = broadcast::channel(64);
        let inner = Arc::new(Inner {
            node,
            device_key,
            my_id,
            nicknames: StdRwLock::new(nicknames),
            data_dir,
            state: Mutex::new(State {
                config: None,
                doc: None,
                author: None,
                roster: Roster::default(),
                presence: PresenceTracker::default(),
                pending: HashMap::new(),
                gossip_sender: None,
            }),
            events,
            conns: StdRwLock::new(HashMap::new()),
            routes: StdRwLock::new(RouteTable::default()),
            tun: StdRwLock::new(None),
            tun_attempted: AtomicBool::new(false),
            remote_access_disabled: AtomicBool::new(prefs.remote_access_disabled),
            hidden: AtomicBool::new(prefs.hidden),
            conntrack: Conntrack::default(),
            coarse_now: AtomicU64::new(0),
            net_tasks: StdMutex::new(Vec::new()),
            was_member: AtomicBool::new(false),
            protocol_version: AtomicU32::new(admission::PROTOCOL_VERSION),
            last_seen_saved: AtomicU64::new(0),
            geo: StdRwLock::new(None),
            geo_downloading: AtomicBool::new(false),
        });

        // Accept loops for our custom ALPNs.
        spawn_accept_loop(inner.clone(), mesh_rx, handle_mesh_incoming);
        spawn_accept_loop(inner.clone(), join_rx, handle_join_incoming);

        // Seed last-seen times from disk so "offline > 1 week" survives restarts.
        {
            let seen = load_last_seen(&inner.data_dir);
            let mut st = inner.state.lock().await;
            for (id, ts) in seen {
                st.presence.set_last_seen(id, ts);
            }
        }

        // Load + activate a stored network, if any.
        if let Some(cfg) = load_config(&inner.data_dir)? {
            activate(&inner, cfg).await?;
        }

        // Periodic maintenance: rebuild roster, dial missing members, presence.
        {
            let inner = inner.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = tick(&inner).await {
                        tracing::debug!("tick error: {e:#}");
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });
        }

        Ok(Engine { inner })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    pub fn self_node_id_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.inner.my_id)
    }

    /// Number of live mesh connections (used by tests to verify no ghost
    /// connections remain after a member is removed or the network is deleted).
    pub fn live_connection_count(&self) -> usize {
        self.inner.conns.read().unwrap().len()
    }

    /// Override this device's mesh/join protocol version (test hook for the
    /// version-mismatch path). In production it stays `admission::PROTOCOL_VERSION`.
    pub fn set_protocol_version(&self, v: u32) {
        self.inner.protocol_version.store(v, Ordering::SeqCst);
    }

    pub async fn has_network(&self) -> bool {
        self.inner.state.lock().await.config.is_some()
    }

    /// Create a new network; this device becomes the originator. Returns the
    /// join ticket.
    pub async fn create_network(&self, name: String, subnet: Ipv4Addr) -> Result<String> {
        {
            let st = self.inner.state.lock().await;
            if st.config.is_some() {
                bail!("this device already belongs to a network");
            }
        }
        let secret = NetworkSecret::generate();
        let originator = generate_originator_key();
        let originator_id = originator.verifying_key().to_bytes();

        let cfg = StoredConfig {
            name: name.clone(),
            subnet: subnet.octets(),
            secret: secret.to_bytes(),
            originator_id,
            originator_secret: Some(originator.to_bytes()),
        };
        save_config(&self.inner.data_dir, &cfg)?;
        activate(&self.inner, cfg).await?;

        // Genesis entry: the originator master key vouches its own device in as a
        // Controller at the first host address.
        let genesis = sign(
            secret.network_id(),
            &originator,
            Op::Add {
                node_id: self.inner.my_id,
                hostname: current_hostname(),
                role: Role::Controller,
                virtual_ip: first_host(subnet),
                invite_nonce: [0u8; 16],
                ts: now_ms(),
            },
        );
        publish(&self.inner, &genesis).await?;

        // Seed an initial (reusable) Peer invite so the first ticket is valid.
        let peer_nonce = new_nonce();
        let set_inv = sign(
            secret.network_id(),
            &originator,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce: peer_nonce,
                single_use: false,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &set_inv).await?;

        let ticket = Ticket::new(
            name,
            subnet,
            &secret,
            originator_id,
            self.inner.node.addr(),
            InviteKind::Peer,
            peer_nonce,
        );
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(ticket.encode())
    }

    /// Join an existing network using a ticket. Drives the SAS flow: emits a
    /// [`EngineEvent::JoinSas`] then blocks until an existing member approves.
    pub async fn join_network(&self, ticket_str: &str) -> Result<()> {
        let ticket = Ticket::decode(ticket_str)?;
        {
            let st = self.inner.state.lock().await;
            if st.config.is_some() {
                bail!("this device already belongs to a network");
            }
        }
        let secret = ticket.secret();
        // Store a provisional config and open the shared roster doc.
        let cfg = StoredConfig {
            name: ticket.name.clone(),
            subnet: ticket.subnet,
            secret: secret.to_bytes(),
            originator_id: ticket.originator_id,
            originator_secret: None,
        };
        // Run the handshake WITHOUT activating yet: we only open the network once
        // we're accepted, so the joiner never shows a network while pending and a
        // decline leaves it cleanly at no-network. If activation/persist fails
        // *after* acceptance, tear it back down.
        match join_handshake(&self.inner, &cfg, &ticket).await {
            Ok(()) => Ok(()),
            Err(e) => {
                teardown(&self.inner).await;
                Err(e)
            }
        }
    }

    /// Approve a pending join request (member side).
    pub async fn approve_join(&self, node_id_hex: &str) -> Result<()> {
        self.decide_join(node_id_hex, true).await
    }

    /// Deny a pending join request (member side).
    pub async fn deny_join(&self, node_id_hex: &str) -> Result<()> {
        self.decide_join(node_id_hex, false).await
    }

    async fn decide_join(&self, node_id_hex: &str, approve: bool) -> Result<()> {
        let id = parse_id(node_id_hex)?;
        let pending = {
            let mut st = self.inner.state.lock().await;
            st.pending.remove(&id)
        };
        let pending = pending.ok_or_else(|| anyhow!("no pending join from that device"))?;
        let _ = pending.responder.send(approve);
        Ok(())
    }

    /// Remove a member. The originator (master key) can remove anyone; a
    /// Controller signs with its device key and the roster rules let it evict only
    /// Peers. A Peer's attempt is signed but rejected by the fold.
    pub async fn remove_member(&self, node_id_hex: &str) -> Result<()> {
        let id = parse_id(node_id_hex)?;
        let (net_id, key) = self.admin_signer().await?;
        let entry = sign(
            net_id,
            &key,
            Op::Remove {
                node_id: id,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &entry).await?;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Originator-only: promote/demote a member between Peer and Controller.
    pub async fn set_member_role(&self, node_id_hex: &str, controller: bool) -> Result<()> {
        let id = parse_id(node_id_hex)?;
        let originator = self.originator_key().await?;
        let net_id = {
            let st = self.inner.state.lock().await;
            st.config.as_ref().context("no network")?.secret().network_id()
        };
        let entry = sign(
            net_id,
            &originator,
            Op::SetRole {
                node_id: id,
                role: if controller { Role::Controller } else { Role::Peer },
                ts: now_ms(),
            },
        );
        publish(&self.inner, &entry).await?;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Originator-only: freeze or unfreeze the membership roll.
    pub async fn set_frozen(&self, frozen: bool) -> Result<()> {
        let originator = self.originator_key().await?;
        let net_id = {
            let st = self.inner.state.lock().await;
            st.config.as_ref().unwrap().secret().network_id()
        };
        let entry = sign(
            net_id,
            &originator,
            Op::Freeze {
                frozen,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &entry).await?;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Originator-only: dissolve the network. Removes every other member (signed,
    /// so the removals propagate to anyone still connected and boot them — they
    /// can no longer see each other over the link), then leaves locally.
    pub async fn delete_network(&self) -> Result<()> {
        let originator = self.originator_key().await?; // errors unless originator
        let (net_id, others) = {
            let st = self.inner.state.lock().await;
            let net_id = st.config.as_ref().unwrap().secret().network_id();
            let others: Vec<Id> = st
                .roster
                .members()
                .map(|(id, _)| *id)
                .filter(|id| *id != self.inner.my_id)
                .collect();
            (net_id, others)
        };
        for id in others {
            let entry = sign(
                net_id,
                &originator,
                Op::Remove {
                    node_id: id,
                    ts: now_ms(),
                },
            );
            let _ = publish(&self.inner, &entry).await;
        }
        // Give the removals time to sync to still-connected peers before we tear
        // down. Members connected to each other re-propagate via docs, so reaching
        // any one online member is enough for the rest to converge.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        teardown(&self.inner).await;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Originator-only: **rotate the network secret** (mass-revoke). Boots every
    /// current member off the old network, then restarts under a brand-new secret
    /// (same network name, subnet, and originator identity). Anyone holding the
    /// old ticket — including a member who was offline during a removal — is
    /// permanently locked out: they can't derive the new PSK, can't find the new
    /// rendezvous, and can't open the new roster namespace. Returns the new join
    /// ticket to redistribute to the devices you want to keep (they re-join with
    /// the normal SAS flow).
    pub async fn rotate_network(&self) -> Result<String> {
        let originator = self.originator_key().await?;
        let (old_net_id, others, name, subnet, originator_id) = {
            let st = self.inner.state.lock().await;
            let cfg = st.config.as_ref().context("no network")?;
            let others: Vec<Id> = st
                .roster
                .members()
                .map(|(id, _)| *id)
                .filter(|id| *id != self.inner.my_id)
                .collect();
            (
                cfg.secret().network_id(),
                others,
                cfg.name.clone(),
                cfg.subnet(),
                cfg.originator_id,
            )
        };

        // 1. Boot everyone off the OLD network (signed removals propagate to peers
        //    still connected; the ghost-connection fix tears their links down).
        for id in others {
            let entry = sign(
                old_net_id,
                &originator,
                Op::Remove {
                    node_id: id,
                    ts: now_ms(),
                },
            );
            let _ = publish(&self.inner, &entry).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // 2. Tear down the old network entirely.
        teardown(&self.inner).await;

        // 3. Start fresh under a new secret, keeping name/subnet/originator.
        let secret = NetworkSecret::generate();
        let cfg = StoredConfig {
            name: name.clone(),
            subnet: subnet.octets(),
            secret: secret.to_bytes(),
            originator_id,
            originator_secret: Some(originator.to_bytes()),
        };
        save_config(&self.inner.data_dir, &cfg)?;
        activate(&self.inner, cfg).await?;

        // Genesis self-add in the NEW namespace, as a Controller at the first host.
        let genesis = sign(
            secret.network_id(),
            &originator,
            Op::Add {
                node_id: self.inner.my_id,
                hostname: current_hostname(),
                role: Role::Controller,
                virtual_ip: first_host(subnet),
                invite_nonce: [0u8; 16],
                ts: now_ms(),
            },
        );
        publish(&self.inner, &genesis).await?;

        // Seed a fresh Peer invite for the new network.
        let peer_nonce = new_nonce();
        let set_inv = sign(
            secret.network_id(),
            &originator,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce: peer_nonce,
                single_use: false,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &set_inv).await?;

        let ticket = Ticket::new(
            name,
            subnet,
            &secret,
            originator_id,
            self.inner.node.addr(),
            InviteKind::Peer,
            peer_nonce,
        );
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(ticket.encode())
    }

    /// Export this device's originator master key as a recovery code, to back up
    /// (the authority survives device loss) or move to another device. Originator
    /// only.
    pub async fn export_originator_key(&self) -> Result<String> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let secret = cfg
            .originator_secret
            .context("this device does not hold the originator master key")?;
        Ok(encode_recovery_key(&secret))
    }

    /// Import an originator recovery code, granting this device originator powers
    /// for the network it's already in. The code must match this network's
    /// originator (you can't graft a different network's authority on).
    pub async fn import_originator_key(&self, recovery: &str) -> Result<()> {
        let secret = decode_recovery_key(recovery)?;
        let originator_pub = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        let mut cfg = {
            let st = self.inner.state.lock().await;
            st.config.clone().context("no network on this device")?
        };
        if originator_pub != cfg.originator_id {
            bail!("this recovery code is for a different network");
        }
        cfg.originator_secret = Some(secret);
        save_config(&self.inner.data_dir, &cfg)?;
        self.inner.state.lock().await.config = Some(cfg);
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Leave the network on this device only (local teardown; does not affect
    /// other members). Available to any member.
    pub async fn leave_network(&self) -> Result<()> {
        {
            let st = self.inner.state.lock().await;
            if st.config.is_none() {
                bail!("not in a network");
            }
        }
        teardown(&self.inner).await;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Connect to / disconnect from the network without forgetting it. Used by the
    /// GUI: "Quit Nullgate" disconnects (the device goes offline from the pool) but
    /// keeps the config; reopening the app reconnects. Idempotent.
    pub async fn set_online(&self, online: bool) -> Result<()> {
        let inner = &self.inner;
        if online {
            if inner.state.lock().await.doc.is_some() {
                return Ok(()); // already connected
            }
            let cfg = inner
                .state
                .lock()
                .await
                .config
                .clone()
                .or_else(|| load_config(&inner.data_dir).ok().flatten())
                .context("no network to connect to")?;
            activate(inner, cfg).await?;
        } else {
            soft_disconnect(inner).await;
        }
        let _ = inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    async fn originator_key(&self) -> Result<SigningKey> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let secret = cfg
            .originator_secret
            .context("this device does not hold the originator master key")?;
        Ok(SigningKey::from_bytes(&secret))
    }

    /// The key + network id to sign an administrative op with: the originator
    /// master key if this device holds it (full authority), else this device's key
    /// (a Controller, constrained by the fold rules). Errors if this device is a
    /// Peer.
    async fn admin_signer(&self) -> Result<(Id, SigningKey)> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let net_id = cfg.secret().network_id();
        if let Some(secret) = cfg.originator_secret {
            return Ok((net_id, SigningKey::from_bytes(&secret)));
        }
        if st.roster.role(&self.inner.my_id) == Role::Controller {
            return Ok((net_id, self.inner.device_key.clone()));
        }
        bail!("this device is a Peer and can't perform administrative actions");
    }

    /// Publish a fresh Peer invite (originator or Controller signed), superseding
    /// the previous one so old Peer tickets stop admitting new devices.
    async fn regenerate_peer_invite(&self, single_use: bool) -> Result<Nonce> {
        let (net_id, key) = self.admin_signer().await?;
        let nonce = new_nonce();
        let entry = sign(
            net_id,
            &key,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce,
                single_use,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &entry).await?;
        Ok(nonce)
    }

    /// The **Peer-level** join ticket (Controllers and the originator only). Uses
    /// the network's current Peer invite, so showing it repeatedly is stable.
    pub async fn ticket(&self) -> Result<String> {
        let (name, subnet, secret, originator_id, existing) = {
            let st = self.inner.state.lock().await;
            let cfg = st.config.as_ref().context("no network")?;
            let can = cfg.originator_secret.is_some()
                || st.roster.role(&self.inner.my_id) == Role::Controller;
            if !can {
                bail!("only controllers and the originator can view the join ticket");
            }
            (
                cfg.name.clone(),
                cfg.subnet(),
                cfg.secret(),
                cfg.originator_id,
                st.roster.current_invite(InviteKind::Peer),
            )
        };
        let nonce = match existing {
            Some((n, _)) => n,
            None => self.regenerate_peer_invite(false).await?,
        };
        Ok(Ticket::new(
            name,
            subnet,
            &secret,
            originator_id,
            self.inner.node.addr(),
            InviteKind::Peer,
            nonce,
        )
        .encode())
    }

    /// A **Controller-level** join ticket (originator only). Always single-use:
    /// each call mints a fresh nonce that the fold consumes after one admission.
    pub async fn controller_ticket(&self) -> Result<String> {
        let originator = self.originator_key().await?; // originator-only gate
        let (name, subnet, secret, originator_id, net_id) = {
            let st = self.inner.state.lock().await;
            let cfg = st.config.as_ref().context("no network")?;
            (
                cfg.name.clone(),
                cfg.subnet(),
                cfg.secret(),
                cfg.originator_id,
                cfg.secret().network_id(),
            )
        };
        let nonce = new_nonce();
        let entry = sign(
            net_id,
            &originator,
            Op::SetInvite {
                kind: InviteKind::Controller,
                nonce,
                single_use: true,
                ts: now_ms(),
            },
        );
        publish(&self.inner, &entry).await?;
        Ok(Ticket::new(
            name,
            subnet,
            &secret,
            originator_id,
            self.inner.node.addr(),
            InviteKind::Controller,
            nonce,
        )
        .encode())
    }

    /// Toggle whether Peer join tickets are single-use. Either change mints a new
    /// Peer invite, immediately invalidating any previously-shared Peer code (for
    /// *new* joins — current members keep their access).
    pub async fn set_peer_ticket_single_use(&self, on: bool) -> Result<()> {
        self.regenerate_peer_invite(on).await?;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Toggle this device's one-way inbound block. Outbound access still works.
    pub async fn set_remote_access_disabled(&self, disabled: bool) -> Result<()> {
        self.inner
            .remote_access_disabled
            .store(disabled, Ordering::Relaxed);
        self.persist_prefs();
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Toggle whether this device hides itself from the member list. Hiding
    /// implies the inbound block (the effective block is `disabled || hidden`).
    pub async fn set_hidden(&self, hidden: bool) -> Result<()> {
        self.inner.hidden.store(hidden, Ordering::Relaxed);
        self.persist_prefs();
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    fn persist_prefs(&self) {
        save_device_prefs(
            &self.inner.data_dir,
            &DevicePrefs {
                remote_access_disabled: self.inner.remote_access_disabled.load(Ordering::Relaxed),
                hidden: self.inner.hidden.load(Ordering::Relaxed),
            },
        );
    }

    /// The administration activity log: a 30-day, human-readable view derived from
    /// the signed roster history. Visible to every member.
    pub async fn audit_log(&self) -> Result<Vec<AuditEntry>> {
        let (doc, originator_id) = {
            let st = self.inner.state.lock().await;
            let cfg = st.config.as_ref().context("no network")?;
            (st.doc.clone().context("network offline")?, cfg.originator_id)
        };
        let entries = membership::load_entries(&doc, self.inner.node.blobs.blobs()).await?;

        // Resolve friendly names from Add hostnames + this client's local nicknames.
        let mut hostnames: HashMap<Id, String> = HashMap::new();
        for e in &entries {
            if let Op::Add { node_id, hostname, .. } = &e.op {
                hostnames.insert(*node_id, hostname.clone());
            }
        }
        let nicks = self.inner.nicknames.read().unwrap();
        let name_of = |id: &Id| -> Option<String> {
            let hex = data_encoding::HEXLOWER.encode(id);
            nicks.get(&hex).cloned().or_else(|| hostnames.get(id).cloned())
        };
        let label = |id: &Id| name_of(id).unwrap_or_else(|| short(id));

        let cutoff = now_ms().saturating_sub(30 * 24 * 3600 * 1000);
        let mut out: Vec<AuditEntry> = Vec::new();
        for e in &entries {
            let ts = e.op.ts();
            if ts < cutoff {
                continue;
            }
            let action = match &e.op {
                Op::Add { node_id, role, .. } => {
                    format!("Added {} as {}", label(node_id), role.as_str())
                }
                Op::Remove { node_id, .. } => format!("Removed {}", label(node_id)),
                Op::SetRole { node_id, role, .. } => {
                    format!("Set {} to {}", label(node_id), role.as_str())
                }
                Op::SetInvite { kind, single_use, .. } => match kind {
                    InviteKind::Peer => format!(
                        "Regenerated the Peer join code{}",
                        if *single_use { " (single-use)" } else { "" }
                    ),
                    InviteKind::Controller => "Issued a Controller join code (single-use)".into(),
                },
                Op::Freeze { frozen, .. } => {
                    if *frozen {
                        "Froze membership".into()
                    } else {
                        "Unfroze membership".into()
                    }
                }
                Op::SetName { name, .. } => format!("Renamed the network to \"{name}\""),
            };
            let (actor_node_id, actor_name) = if e.signer == originator_id {
                (
                    "originator".to_string(),
                    Some("Originator (master key)".to_string()),
                )
            } else {
                (data_encoding::HEXLOWER.encode(&e.signer), name_of(&e.signer))
            };
            out.push(AuditEntry {
                ts,
                actor_node_id,
                actor_name,
                action,
            });
        }
        drop(nicks);
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        Ok(out)
    }

    /// Rename the network. The name is shared: it's published to the signed roster
    /// (any current member may set it, last-writer-wins) so every device converges
    /// to the same name.
    pub async fn set_network_name(&self, name: String) -> Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            bail!("network name can't be empty");
        }
        let net_id = {
            let st = self.inner.state.lock().await;
            st.config.as_ref().context("no network")?.secret().network_id()
        };
        let entry = sign(
            net_id,
            &self.inner.device_key,
            Op::SetName { name, ts: now_ms() },
        );
        publish(&self.inner, &entry).await?;
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Set (or clear, with `None`/empty) this client's **local** friendly nickname
    /// for another member. Stored locally and never broadcast; the OS hostname is
    /// the shared identifier.
    pub async fn set_nickname(&self, node_id_hex: &str, name: Option<String>) -> Result<()> {
        let _ = parse_id(node_id_hex)?; // validate it's a real NodeId hex
        let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        {
            let mut map = self.inner.nicknames.write().unwrap();
            match name {
                Some(n) => {
                    map.insert(node_id_hex.to_string(), n);
                }
                None => {
                    map.remove(node_id_hex);
                }
            }
            save_nicknames(&self.inner.data_dir, &map);
        }
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Snapshot of the network for display.
    pub async fn status(&self) -> Result<NetworkStatus> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let nicks = self.inner.nicknames.read().unwrap();
        let self_addr = self.inner.node.addr();
        let (self_local, self_public) = split_local_public(self_addr.ip_addrs().copied());
        let is_orig = cfg.originator_secret.is_some();
        let mut members = Vec::new();
        for (id, m) in st.roster.members() {
            let ps = st.presence.get(id);
            let is_self = *id == self.inner.my_id;
            let node_hex = data_encoding::HEXLOWER.encode(id);
            // Per-device flags: self reads its own live toggles; others come from
            // the signed presence heartbeat.
            let access_disabled = if is_self {
                self.inner.remote_access_disabled.load(Ordering::Relaxed)
            } else {
                ps.map(|p| p.access_disabled).unwrap_or(false)
            };
            let hidden = if is_self {
                self.inner.hidden.load(Ordering::Relaxed)
            } else {
                ps.map(|p| p.hidden).unwrap_or(false)
            };
            // "Hide" is a courtesy: filter hidden members out for non-originator
            // viewers. The member is still routed (enforcement is the inbound block).
            if !is_self && hidden && !is_orig {
                continue;
            }
            let role = if is_self && is_orig {
                "originator".to_string()
            } else {
                st.roster.role(id).as_str().to_string()
            };
            let (local_ip, public_ip) = if is_self {
                (self_local.clone(), self_public.clone())
            } else {
                (
                    ps.and_then(|p| p.local_ip.clone()),
                    ps.and_then(|p| p.public_ip.clone()),
                )
            };
            members.push(MemberView {
                hostname: if is_self {
                    Some(current_hostname())
                } else {
                    ps.and_then(|p| p.hostname.clone())
                        .or_else(|| Some(m.hostname.clone()))
                },
                // Local nickname this client set for the member (never shared).
                label: nicks.get(&node_hex).cloned(),
                virtual_ip: Some(m.virtual_ip.to_string()),
                local_ip,
                public_ip,
                location: ps.and_then(|p| p.location.clone()),
                observed_addr: ps.and_then(|p| p.observed_addr.clone()),
                direct: ps.and_then(|p| p.direct),
                online: is_self || ps.map(|p| p.online).unwrap_or(false),
                last_seen: ps.map(|p| p.last_seen).unwrap_or(0),
                is_self,
                is_originator_device: m.added_by == cfg.originator_id && false, // device==originator-master only at genesis; informational
                role,
                access_disabled,
                hidden,
                node_id: node_hex,
            });
        }
        drop(nicks);
        // Order: online (0) before access-disabled (1) before offline (2); hidden
        // members (only ever shown to the originator) sink to the bottom (3).
        let rank = |m: &MemberView| -> u8 {
            if m.hidden {
                3
            } else if !m.online {
                2
            } else if m.access_disabled {
                1
            } else {
                0
            }
        };
        members.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.node_id.cmp(&b.node_id)));
        let self_role = if is_orig {
            "originator".to_string()
        } else {
            st.roster.role(&self.inner.my_id).as_str().to_string()
        };
        let peer_ticket_single_use = st
            .roster
            .current_invite(InviteKind::Peer)
            .map(|(_, su)| su)
            .unwrap_or(false);
        Ok(NetworkStatus {
            // Prefer the shared roster name; fall back to the local config name.
            name: st
                .roster
                .name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| cfg.name.clone()),
            subnet: cfg.subnet().to_string(),
            frozen: st.roster.frozen(),
            self_node_id: data_encoding::HEXLOWER.encode(&self.inner.my_id),
            self_ip: st
                .roster
                .member(&self.inner.my_id)
                .map(|m| m.virtual_ip.to_string()),
            is_originator: cfg.originator_secret.is_some(),
            self_role,
            peer_ticket_single_use,
            routing: self.inner.tun.read().unwrap().is_some(),
            online: st.doc.is_some(),
            home_relay: self
                .inner
                .node
                .addr()
                .relay_urls()
                .next()
                .map(|u| u.to_string()),
            members,
        })
    }
}

// ---------------------------------------------------------------------------
// Activation + background work
// ---------------------------------------------------------------------------

async fn activate(inner: &Arc<Inner>, cfg: StoredConfig) -> Result<()> {
    let secret = cfg.secret();
    // Open the deterministic roster document (same namespace for every member).
    let ns = iroh_docs::NamespaceSecret::from_bytes(&secret.docs_namespace_seed());
    let doc = inner
        .node
        .docs_api()
        .import_namespace(iroh_docs::Capability::Write(ns))
        .await
        .context("open roster doc")?;
    let author = inner.node.docs_api().author_create().await?;

    // Subscribe to presence gossip on the private rendezvous topic.
    let topic = TopicId::from_bytes(secret.rendezvous());
    let sub = inner
        .node
        .gossip
        .subscribe(topic, Vec::<EndpointId>::new())
        .await
        .context("presence subscribe")?;
    let (sender, mut receiver) = sub.split();
    {
        let ti = inner.clone();
        let net_id = secret.network_id();
        let originator_id = cfg.originator_id;
        let h = tokio::spawn(async move {
            while let Some(ev) = receiver.next().await {
                let Ok(ev) = ev else { continue };
                let Event::Received(m) = ev else { continue };
                let Ok(msg) = ciborium::from_reader::<GossipMsg, _>(m.content.as_ref()) else {
                    continue;
                };
                match msg {
                    GossipMsg::Presence(p) => {
                        if p.verify(&net_id) && p.node_id != ti.my_id {
                            let mut st = ti.state.lock().await;
                            st.presence.record_heartbeat(
                                p.node_id,
                                p.hostname,
                                p.public_ip,
                                p.remote_access_disabled,
                                p.hidden,
                                p.ts,
                            );
                            drop(st);
                            let _ = ti.events.send(EngineEvent::Changed);
                        }
                    }
                    GossipMsg::Locations(loc) => {
                        // Trust only the originator's signed location assertions.
                        if loc.verify(&net_id, &originator_id) {
                            let mut st = ti.state.lock().await;
                            for (id, location) in loc.entries {
                                st.presence.set_location(id, Some(location));
                            }
                            drop(st);
                            let _ = ti.events.send(EngineEvent::Changed);
                        }
                    }
                }
            }
        });
        inner.net_tasks.lock().unwrap().push(h.abort_handle());
    }

    let mut st = inner.state.lock().await;
    st.config = Some(cfg);
    st.doc = Some(doc);
    st.author = Some(author);
    st.gossip_sender = Some(sender);
    Ok(())
}

/// Drop the live network state on this device (stop network-scoped tasks, drop
/// the TUN/interface, close mesh connections, clear in-memory live state) while
/// **keeping** the persisted config so it can reconnect. This is "go offline".
async fn soft_disconnect(inner: &Arc<Inner>) {
    for h in inner.net_tasks.lock().unwrap().drain(..) {
        h.abort();
    }
    *inner.tun.write().unwrap() = None;
    inner.tun_attempted.store(false, Ordering::SeqCst);
    inner.was_member.store(false, Ordering::SeqCst);
    inner.conntrack.clear();
    *inner.routes.write().unwrap() = RouteTable::default();
    let conns: Vec<Connection> = {
        let mut map = inner.conns.write().unwrap();
        map.drain().map(|(_, c)| c).collect()
    };
    for c in conns {
        c.close(0u32.into(), b"disconnected");
    }
    let mut st = inner.state.lock().await;
    st.doc = None;
    st.author = None;
    st.roster = Roster::default();
    st.presence = PresenceTracker::default();
    st.gossip_sender = None;
    st.pending.clear();
    // st.config is intentionally kept.
}

/// Fully leave a network: go offline, then forget the config (in memory, on disk,
/// and in the keystore). The device key is kept (it's network-independent).
async fn teardown(inner: &Arc<Inner>) {
    soft_disconnect(inner).await;
    inner.state.lock().await.config = None;
    let _ = std::fs::remove_file(config_path(&inner.data_dir));
    crate::secrets::delete(&inner.data_dir, KEY_NETWORK_SECRET);
    crate::secrets::delete(&inner.data_dir, KEY_ORIGINATOR_SECRET);
}

/// One maintenance pass: rebuild the roster from the doc, refresh route/presence,
/// dial any missing members, and broadcast our presence.
async fn tick(inner: &Arc<Inner>) -> Result<()> {
    // Refresh the coarse clock the per-packet pump reads, and trim stale conntrack
    // flows (used by the one-way "disable remote access" block).
    let now_coarse = now_ms();
    inner.coarse_now.store(now_coarse, Ordering::Relaxed);
    inner.conntrack.sweep(now_coarse);

    let (doc, cfg) = {
        let st = inner.state.lock().await;
        match (st.doc.clone(), st.config.clone()) {
            (Some(d), Some(c)) => (d, c),
            _ => return Ok(()),
        }
    };

    // Rebuild roster from the document, and the forwarding table from it.
    let roster = membership::build_roster(&cfg.roster_cfg(), &doc, inner.node.blobs.blobs()).await?;

    // Self-eviction: once we've appeared in the roster, dropping out of it means
    // we were removed (single remove, network delete, or secret rotation) — leave
    // cleanly so we hold no connections and stop showing the (now dead) network.
    if roster.is_member(&inner.my_id) {
        inner.was_member.store(true, Ordering::SeqCst);
    } else if inner.was_member.load(Ordering::SeqCst) {
        tracing::info!("this device was removed from the network — leaving");
        teardown(inner).await;
        let _ = inner.events.send(EngineEvent::Changed);
        return Ok(());
    }

    *inner.routes.write().unwrap() = RouteTable::from_roster(&roster);

    // Enforce membership on the live mesh continuously: close any connection to a
    // peer who is no longer a current member (e.g. removed by the originator or
    // after the network was deleted). Without this, a peer removed *after* it
    // connected could keep a "ghost" connection. Routing already excludes them
    // (they're not in the route table), and they can't re-dial (membership gate),
    // but we proactively tear the transport down so visibility truly ends.
    let members: std::collections::HashSet<Id> = roster.members().map(|(id, _)| *id).collect();
    let stale: Vec<(Id, Connection)> = inner
        .conns
        .read()
        .unwrap()
        .iter()
        .filter(|(id, _)| !members.contains(*id))
        .map(|(id, c)| (*id, c.clone()))
        .collect();
    for (id, c) in stale {
        tracing::debug!("closing ghost connection to non-member {}", short(&id));
        c.close(0u32.into(), b"no longer a member");
        inner.conns.write().unwrap().remove(&id);
        inner
            .state
            .lock()
            .await
            .presence
            .record_connection(id, None, None, None, None, false);
    }

    // Bring up the TUN with our roster-assigned IP (best-effort; needs elevation).
    if let Some(me) = roster.member(&inner.my_id) {
        enable_tun(inner, me.virtual_ip).await;
    }

    // Snapshot members to dial + update presence virtual IPs.
    let connected: std::collections::HashSet<Id> =
        inner.conns.read().unwrap().keys().copied().collect();
    let mut to_dial: Vec<Id> = Vec::new();
    {
        let mut st = inner.state.lock().await;
        for (id, m) in roster.members() {
            st.presence.set_virtual_ip(*id, m.virtual_ip);
            if *id != inner.my_id && !connected.contains(id) {
                to_dial.push(*id);
            }
        }
        st.roster = roster.clone();
    }

    // Dial missing members (off-lock).
    for peer in to_dial {
        let inner = inner.clone();
        let psk = cfg.secret().psk();
        tokio::spawn(async move {
            if let Err(e) = dial_member(&inner, peer, psk).await {
                tracing::debug!("dial {} failed: {e:#}", short(&peer));
            }
        });
    }

    // Broadcast presence + grow the gossip mesh toward all members.
    let (sender, peers) = {
        let st = inner.state.lock().await;
        let peers: Vec<EndpointId> = roster
            .members()
            .filter_map(|(id, _)| {
                if *id == inner.my_id {
                    None
                } else {
                    EndpointId::from_bytes(id).ok()
                }
            })
            .collect();
        (st.gossip_sender.clone(), peers)
    };
    if let Some(sender) = sender {
        // Advertise our own public IP (same source the self view uses) so peers
        // can show it even over a relay path where they can't observe it directly.
        let my_public_ip = {
            let addr = inner.node.addr();
            split_local_public(addr.ip_addrs().copied()).1
        };
        let rad = inner.remote_access_disabled.load(Ordering::Relaxed);
        let hid = inner.hidden.load(Ordering::Relaxed);
        let p = Presence::signed(
            cfg.secret().network_id(),
            &inner.device_key,
            current_hostname(),
            my_public_ip.clone(),
            rad || hid, // hide implies the inbound block
            hid,
            now_ms(),
        );
        let mut buf = Vec::new();
        let _ = ciborium::into_writer(&GossipMsg::Presence(p), &mut buf);
        let _ = sender.broadcast(Bytes::from(buf)).await;

        // Originator only: resolve each member's public IP to a location and
        // propagate a signed map so members can show it without the DB.
        if cfg.originator_secret.is_some() {
            // Gather (node_id, public_ip) for everyone (under the async lock).
            let pairs: Vec<(Id, Option<String>)> = {
                let st = inner.state.lock().await;
                roster
                    .members()
                    .map(|(id, _)| {
                        let pip = if *id == inner.my_id {
                            my_public_ip.clone()
                        } else {
                            st.presence.get(id).and_then(|p| p.public_ip.clone())
                        };
                        (*id, pip)
                    })
                    .collect()
            };
            // Resolve synchronously (don't hold the geo lock across an await).
            let entries: Vec<(Id, String)> = {
                let guard = inner.geo.read().unwrap();
                match guard.as_ref() {
                    Some(geo) => pairs
                        .into_iter()
                        .filter_map(|(id, pip)| {
                            let ip: std::net::Ipv4Addr = pip?.parse().ok()?;
                            geo.lookup(ip).map(|loc| (id, loc))
                        })
                        .collect(),
                    None => Vec::new(),
                }
            };
            if !entries.is_empty() {
                let orig = SigningKey::from_bytes(&cfg.originator_secret.unwrap());
                let loc =
                    Locations::signed(cfg.secret().network_id(), &orig, entries.clone(), now_ms());
                let mut lbuf = Vec::new();
                let _ = ciborium::into_writer(&GossipMsg::Locations(loc), &mut lbuf);
                let _ = sender.broadcast(Bytes::from(lbuf)).await;
                // Apply to our own view (gossip doesn't loop back to us).
                let mut st = inner.state.lock().await;
                for (id, l) in entries {
                    st.presence.set_location(id, Some(l));
                }
            }
        }

        if !peers.is_empty() {
            let _ = sender.join_peers(peers).await;
        }
    }

    // Originator: keep the geo DB present + fresh (downloads in the background).
    ensure_geo(inner).await;

    // Refresh observed-address / direct info for live peers.
    let live: Vec<Id> = inner.conns.read().unwrap().keys().copied().collect();
    for peer in live {
        let ci = conn_info(inner, &peer).await;
        let mut st = inner.state.lock().await;
        st.presence
            .record_connection(peer, ci.observed, ci.direct, ci.local_ip, ci.public_ip, true);
    }

    // Persist last-seen (throttled) so "offline > 1 week" survives restarts.
    let now = now_ms();
    if now.saturating_sub(inner.last_seen_saved.load(Ordering::Relaxed)) > 30_000 {
        inner.last_seen_saved.store(now, Ordering::Relaxed);
        let map: std::collections::HashMap<String, u64> = {
            let st = inner.state.lock().await;
            st.presence
                .iter()
                .filter(|(_, p)| p.last_seen > 0)
                .map(|(id, p)| (data_encoding::HEXLOWER.encode(id), p.last_seen))
                .collect()
        };
        save_last_seen(&inner.data_dir, &map);
    }

    let _ = inner.events.send(EngineEvent::Changed);
    Ok(())
}

async fn dial_member(inner: &Arc<Inner>, peer: Id, psk: [u8; 32]) -> Result<()> {
    let addr = EndpointAddr::from_parts(
        EndpointId::from_bytes(&peer).context("bad peer id")?,
        Vec::<TransportAddr>::new(),
    );
    let conn = inner.node.endpoint.connect(addr, MESH_ALPN).await?;
    let _verified = admission::dial(
        &conn,
        inner.my_id,
        &psk,
        inner.protocol_version.load(Ordering::SeqCst),
    )
    .await?;
    register_mesh(inner, peer, conn).await;
    Ok(())
}

async fn handle_mesh_incoming(inner: Arc<Inner>, conn: Connection) {
    let psk = match current_psk(&inner).await {
        Some(p) => p,
        None => return,
    };
    let peer = match admission::accept(
        &conn,
        inner.my_id,
        &psk,
        inner.protocol_version.load(Ordering::SeqCst),
    )
    .await
    {
        Ok(v) => v.peer_id,
        Err(e) => {
            tracing::debug!("mesh handshake failed: {e:#}");
            // Linger briefly so a rejected peer (e.g. version mismatch) can read
            // our finished handshake frame and surface a clear error.
            tokio::time::sleep(Duration::from_millis(300)).await;
            return;
        }
    };
    // Roster gate: only route for current members.
    {
        let st = inner.state.lock().await;
        if !st.roster.is_member(&peer) {
            tracing::debug!("rejecting non-member {}", short(&peer));
            return;
        }
    }
    register_mesh(&inner, peer, conn).await;
}

async fn handle_join_incoming(inner: Arc<Inner>, conn: Connection) {
    let (psk, net_id) = {
        let st = inner.state.lock().await;
        match st.config.as_ref() {
            Some(c) => (c.secret().psk(), c.secret().network_id()),
            None => return,
        }
    };
    let verified = match admission::accept(
        &conn,
        inner.my_id,
        &psk,
        inner.protocol_version.load(Ordering::SeqCst),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("join handshake failed: {e:#}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            return;
        }
    };
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("join accept_bi failed: {e:#}");
            return;
        }
    };
    let req: JoinRequest = match read_msg(&mut recv).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("join read request failed: {e:#}");
            return;
        }
    };

    // Register a pending decision and surface it to the UI.
    let (tx, rx) = oneshot::channel();
    {
        let mut st = inner.state.lock().await;
        st.pending.insert(verified.peer_id, PendingJoin { responder: tx });
    }
    let _ = inner.events.send(EngineEvent::JoinRequest {
        node_id: data_encoding::HEXLOWER.encode(&verified.peer_id),
        hostname: req.hostname.clone(),
        sas: verified.sas.iter().map(|s| s.to_string()).collect(),
    });

    let approved = rx.await.unwrap_or(false);
    let resp = if approved {
        match admit_member(
            &inner,
            net_id,
            verified.peer_id,
            req.hostname,
            req.invite_kind,
            req.invite_nonce,
        )
        .await
        {
            Ok(()) => {
                // Help the joiner sync the roster from us.
                if let Some(doc) = inner.state.lock().await.doc.clone() {
                    if let Ok(a) = bootstrap_addr(&verified.peer_id) {
                        let _ = doc.start_sync(vec![a]).await;
                    }
                }
                JoinResponse::Approved
            }
            Err(e) => {
                tracing::warn!("admit failed: {e:#}");
                JoinResponse::Denied
            }
        }
    } else {
        JoinResponse::Denied
    };
    if let Err(e) = write_msg(&mut send, &resp).await {
        tracing::debug!("join write response failed: {e:#}");
    }
    // Flush the response and keep the connection alive until the joiner has read
    // it and closed, so dropping our handle doesn't reset the stream first.
    let _ = send.finish();
    let _ = inner.events.send(EngineEvent::Changed);
    let _ = conn.closed().await;
}

/// The joiner-side handshake (after provisional activation): dial the bootstrap
/// member, run admission + SAS, request to join, and act on the decision. Returns
/// `Err` on decline or any failure so the caller can tear the activation down.
async fn join_handshake(inner: &Arc<Inner>, cfg: &StoredConfig, ticket: &Ticket) -> Result<()> {
    let secret = cfg.secret();
    let conn = inner
        .node
        .endpoint
        .connect(ticket.bootstrap.clone(), JOIN_ALPN)
        .await
        .context("dial bootstrap member")?;
    let psk = secret.psk();
    let verified = admission::dial(
        &conn,
        inner.my_id,
        &psk,
        inner.protocol_version.load(Ordering::SeqCst),
    )
    .await?;
    let _ = inner.events.send(EngineEvent::JoinSas {
        sas: verified.sas.iter().map(|s| s.to_string()).collect(),
    });

    let (mut send, mut recv) = conn.open_bi().await.context("open join stream")?;
    write_msg(
        &mut send,
        &JoinRequest {
            hostname: current_hostname(),
            invite_kind: ticket.invite_kind,
            invite_nonce: ticket.invite_nonce,
        },
    )
    .await?;
    let resp: JoinResponse = read_msg(&mut recv).await.context("read join response")?;
    match resp {
        JoinResponse::Approved => {
            // Accepted — only now open the network: activate the doc/presence,
            // persist, and pull the roster from the member who admitted us (our
            // virtual IP is derived once we appear in it).
            activate(inner, cfg.clone()).await?;
            save_config(&inner.data_dir, cfg)?;
            if let Some(doc) = inner.state.lock().await.doc.clone() {
                let _ = doc.start_sync(vec![ticket.bootstrap.clone()]).await;
            }
            let _ = inner.events.send(EngineEvent::Changed);
            Ok(())
        }
        JoinResponse::Denied => bail!("the member declined the join request"),
    }
}

/// Write a signed `Add` vouching the joiner in. The role is set by the joiner's
/// ticket kind; the admitter assigns the lowest free virtual IP (recorded in the
/// entry, so it's static) and cites the ticket's invite nonce (the fold validates
/// it against the current invite). Signed with the admitter's device key — the
/// fold rules require the admitter to be a Controller (or originator).
async fn admit_member(
    inner: &Arc<Inner>,
    net_id: Id,
    peer: Id,
    hostname: String,
    invite_kind: InviteKind,
    invite_nonce: Nonce,
) -> Result<()> {
    let (frozen, can_admit, virtual_ip) = {
        let st = inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let can = cfg.originator_secret.is_some()
            || st.roster.role(&inner.my_id) == Role::Controller;
        let ip = st.roster.lowest_free_host(cfg.subnet());
        (st.roster.frozen(), can, ip)
    };
    if frozen {
        bail!("roster is frozen");
    }
    if !can_admit {
        bail!("only controllers and the originator can approve joins");
    }
    let role = match invite_kind {
        InviteKind::Peer => Role::Peer,
        InviteKind::Controller => Role::Controller,
    };
    let entry = sign(
        net_id,
        &inner.device_key,
        Op::Add {
            node_id: peer,
            hostname,
            role,
            virtual_ip: virtual_ip.octets(),
            invite_nonce,
            ts: now_ms(),
        },
    );
    publish(inner, &entry).await
}

async fn register_mesh(inner: &Arc<Inner>, peer: Id, conn: Connection) {
    {
        inner.conns.write().unwrap().insert(peer, conn.clone());
        let mut st = inner.state.lock().await;
        st.presence.record_connection(peer, None, None, None, None, true);
    }
    let _ = inner.events.send(EngineEvent::Changed);

    // Inbound data plane: datagrams from this peer are IP packets — write them to
    // the TUN. Runs until the connection closes.
    {
        let inner = inner.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            loop {
                match conn.read_datagram().await {
                    Ok(pkt) => {
                        let tun = inner.tun.read().unwrap().clone();
                        if let Some(tun) = tun {
                            let mut pkt = pkt.to_vec();
                            // One-way block: when this device disables remote access
                            // (or is hidden), drop inbound that isn't return traffic
                            // for a flow we initiated.
                            let block = inner.remote_access_disabled.load(Ordering::Relaxed)
                                || inner.hidden.load(Ordering::Relaxed);
                            if block
                                && !inner
                                    .conntrack
                                    .allows_inbound(&pkt, inner.coarse_now.load(Ordering::Relaxed))
                            {
                                continue;
                            }
                            // Clamp inbound TCP SYNs too (bounds the other direction).
                            clamp_tcp_mss(&mut pkt, TUN_MSS);
                            let _ = tun.send(&pkt).await;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Watch for close to drop the connection + mark offline.
    let inner2 = inner.clone();
    tokio::spawn(async move {
        let _ = conn.closed().await;
        inner2.conns.write().unwrap().remove(&peer);
        let mut st = inner2.state.lock().await;
        st.presence.record_connection(peer, None, None, None, None, false);
        drop(st);
        let _ = inner2.events.send(EngineEvent::Changed);
    });
}

/// Bring up the OS TUN once, if we know our virtual IP. Best-effort: if it fails
/// (no elevation, missing wintun.dll, …) we log and keep running without routing,
/// so membership + presence still work. Spawns the outbound read loop on success.
async fn enable_tun(inner: &Arc<Inner>, ip: Ipv4Addr) {
    // Escape hatch for tests/CI (and headless runs where a TUN is undesirable).
    if std::env::var_os("NULLGATE_DISABLE_TUN").is_some() {
        return;
    }
    if inner.tun_attempted.swap(true, Ordering::SeqCst) {
        return; // only attempt once
    }
    match RealTun::open(ip, 24, TUN_MTU) {
        Ok(tun) => {
            let tun = Arc::new(tun);
            *inner.tun.write().unwrap() = Some(tun.clone());
            tracing::info!("routing enabled: TUN up at {ip}/24 (mtu {TUN_MTU})");
            let _ = inner.events.send(EngineEvent::Changed);
            // Outbound data plane: TUN packet → dst IP → peer → QUIC datagram.
            let inner2 = inner.clone();
            let h = tokio::spawn(async move {
                let mut buf = vec![0u8; 65535];
                loop {
                    match tun.recv(&mut buf).await {
                        Ok(n) => {
                            let pkt = &mut buf[..n];
                            let Some(dst) = dst_ipv4(pkt) else { continue };
                            // Track this outbound flow so the one-way block lets its
                            // return traffic back in (record before the MSS rewrite).
                            inner2
                                .conntrack
                                .record_outbound(pkt, inner2.coarse_now.load(Ordering::Relaxed));
                            // Clamp TCP MSS so flows stay within the tunnel.
                            clamp_tcp_mss(pkt, TUN_MSS);
                            let peer = inner2.routes.read().unwrap().lookup(&dst);
                            let Some(peer) = peer else { continue };
                            let conn = inner2.conns.read().unwrap().get(&peer).cloned();
                            if let Some(conn) = conn {
                                if let Err(e) = conn.send_datagram(Bytes::copy_from_slice(pkt)) {
                                    tracing::trace!("dropped {}-byte packet: {e}", pkt.len());
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("tun recv ended: {e}");
                            break;
                        }
                    }
                }
            });
            inner.net_tasks.lock().unwrap().push(h.abort_handle());
        }
        Err(e) => {
            tracing::warn!(
                "routing NOT enabled (TUN open failed: {e}). Membership/presence still work; \
                 run elevated (and ensure wintun.dll is present on Windows) for RDP/SSH routing."
            );
        }
    }
}

#[derive(Default)]
struct ConnInfo {
    /// Active address (IP:port or relay URL) — shown as "Observed address".
    observed: Option<String>,
    /// Direct (true) vs relay (false); `None` if unknown.
    direct: Option<bool>,
    /// First known private/LAN IP (no port).
    local_ip: Option<String>,
    /// First known public IP (no port).
    public_ip: Option<String>,
}

/// Inspect what iroh knows about a peer's connection: its active path (direct vs
/// relay + observed addr) and its candidate private/public IP addresses.
async fn conn_info(inner: &Arc<Inner>, peer: &Id) -> ConnInfo {
    let mut out = ConnInfo::default();
    let Ok(eid) = EndpointId::from_bytes(peer) else {
        return out;
    };
    let Some(info) = inner.node.endpoint.remote_info(eid).await else {
        return out;
    };
    use iroh::endpoint::TransportAddrUsage;
    let mut has_direct = false;
    let mut relay = false;
    for a in info.addrs() {
        let active = matches!(a.usage(), TransportAddrUsage::Active);
        match a.addr() {
            TransportAddr::Ip(sa) => {
                let ip = sa.ip();
                if is_private_ip(&ip) {
                    out.local_ip.get_or_insert_with(|| ip.to_string());
                } else if !ip.is_unspecified() {
                    out.public_ip.get_or_insert_with(|| ip.to_string());
                }
                if active {
                    has_direct = true;
                    out.observed = Some(sa.to_string());
                }
            }
            TransportAddr::Relay(url) => {
                if active {
                    relay = true;
                    out.observed.get_or_insert_with(|| url.to_string());
                }
            }
            _ => {}
        }
    }
    out.direct = match (has_direct, relay) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        _ => None,
    };
    out
}

/// Originator-only: make sure the geo DB is loaded, and (re)download it in the
/// background if it's missing or older than two weeks. No-op for non-originators.
async fn ensure_geo(inner: &Arc<Inner>) {
    let is_orig = {
        let st = inner.state.lock().await;
        st.config
            .as_ref()
            .map(|c| c.originator_secret.is_some())
            .unwrap_or(false)
    };
    if !is_orig {
        return;
    }
    let path = inner.data_dir.join(crate::geo::DB_FILENAME);
    let loaded = inner.geo.read().unwrap().is_some();
    let exists = path.exists();

    if exists && !loaded {
        match crate::geo::GeoDb::open(&path) {
            Ok(db) => {
                *inner.geo.write().unwrap() = Some(db);
                tracing::info!("geo db loaded");
            }
            Err(e) => tracing::warn!("geo db load failed: {e:#}"),
        }
    }

    // Download if missing or >14 days old (DB-IP Lite updates monthly).
    let stale = file_older_than(&path, 14 * 24 * 3600);
    if (!exists || stale) && !inner.geo_downloading.swap(true, Ordering::SeqCst) {
        let inner = inner.clone();
        tokio::spawn(async move {
            let p = inner.data_dir.join(crate::geo::DB_FILENAME);
            let dl = tokio::task::spawn_blocking({
                let p = p.clone();
                move || crate::geo::download(&p)
            })
            .await;
            match dl {
                Ok(Ok(())) => match crate::geo::GeoDb::open(&p) {
                    Ok(db) => {
                        *inner.geo.write().unwrap() = Some(db);
                        tracing::info!("geo db downloaded + loaded");
                        let _ = inner.events.send(EngineEvent::Changed);
                    }
                    Err(e) => tracing::warn!("geo db load after download failed: {e:#}"),
                },
                other => tracing::warn!("geo db download failed: {other:?}"),
            }
            inner.geo_downloading.store(false, Ordering::SeqCst);
        });
    }
}

/// Whether a file is missing or older than `secs` seconds (best-effort).
fn file_older_than(path: &Path, secs: u64) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified.elapsed().map(|d| d.as_secs() > secs).unwrap_or(true),
        Err(_) => true,
    }
}

/// Whether an IP is private/LAN (so it's a "Local IP" rather than "Public IP").
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let o = v6.octets();
            v6.is_loopback()
                || (o[0] & 0xfe) == 0xfc // unique-local fc00::/7
                || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80) // link-local fe80::/10
        }
    }
}

/// Partition a set of socket addresses into (first private IP, first public IP),
/// IP only. Used for the local device's own Local/Public IP.
fn split_local_public(
    addrs: impl Iterator<Item = std::net::SocketAddr>,
) -> (Option<String>, Option<String>) {
    let mut local = None;
    let mut public = None;
    for sa in addrs {
        let ip = sa.ip();
        if is_private_ip(&ip) {
            local.get_or_insert_with(|| ip.to_string());
        } else if !ip.is_unspecified() {
            public.get_or_insert_with(|| ip.to_string());
        }
    }
    (local, public)
}

async fn publish(inner: &Arc<Inner>, entry: &crate::roster::Entry) -> Result<()> {
    let (doc, author) = {
        let st = inner.state.lock().await;
        (
            st.doc.clone().context("no doc")?,
            st.author.context("no author")?,
        )
    };
    membership::publish_entry(&doc, author, entry).await
}

async fn current_psk(inner: &Arc<Inner>) -> Option<[u8; 32]> {
    let st = inner.state.lock().await;
    st.config.as_ref().map(|c| c.secret().psk())
}

fn spawn_accept_loop<F, Fut>(inner: Arc<Inner>, mut rx: mpsc::Receiver<Connection>, handler: F)
where
    F: Fn(Arc<Inner>, Connection) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(conn) = rx.recv().await {
            tokio::spawn(handler(inner.clone(), conn));
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bootstrap_addr(id: &Id) -> Result<EndpointAddr> {
    Ok(EndpointAddr::from_parts(
        EndpointId::from_bytes(id).context("bad id")?,
        Vec::<TransportAddr>::new(),
    ))
}

fn parse_id(hex: &str) -> Result<Id> {
    let bytes = data_encoding::HEXLOWER
        .decode(hex.trim().as_bytes())
        .context("node id is not hex")?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .context("node id must be 32 bytes")?;
    Ok(arr)
}

fn short(id: &Id) -> String {
    data_encoding::HEXLOWER.encode(&id[..4])
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE)
}

/// This device's **actual current** OS hostname, read fresh each call so it always
/// reflects the real hostname (never cached, never member-editable).
fn current_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "nullgate-device".into())
}

/// The first usable host address (`.2`) in a /24 subnet.
fn first_host(subnet: Ipv4Addr) -> [u8; 4] {
    let o = subnet.octets();
    [o[0], o[1], o[2], 2]
}

/// A fresh random 16-byte invite nonce.
fn new_nonce() -> Nonce {
    let mut n = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut n);
    n
}

/// Per-device, local-only switch state (never broadcast as config; the live
/// values are also advertised in presence). Persisted next to the other state.
#[derive(Default, Serialize, Deserialize)]
struct DevicePrefs {
    #[serde(default)]
    remote_access_disabled: bool,
    #[serde(default)]
    hidden: bool,
}

fn load_device_prefs(data_dir: &Path) -> DevicePrefs {
    std::fs::read(data_dir.join("device_prefs.cbor"))
        .ok()
        .and_then(|b| ciborium::from_reader(b.as_slice()).ok())
        .unwrap_or_default()
}

fn save_device_prefs(data_dir: &Path, prefs: &DevicePrefs) {
    let _ = std::fs::create_dir_all(data_dir);
    let mut buf = Vec::new();
    if ciborium::into_writer(prefs, &mut buf).is_ok() {
        let _ = std::fs::write(data_dir.join("device_prefs.cbor"), buf);
    }
}

/// This client's local nicknames for other members (NodeId hex → name).
fn load_nicknames(data_dir: &Path) -> HashMap<String, String> {
    std::fs::read(data_dir.join("nicknames.cbor"))
        .ok()
        .and_then(|b| ciborium::from_reader(b.as_slice()).ok())
        .unwrap_or_default()
}

fn save_nicknames(data_dir: &Path, map: &HashMap<String, String>) {
    let _ = std::fs::create_dir_all(data_dir);
    let mut buf = Vec::new();
    if ciborium::into_writer(map, &mut buf).is_ok() {
        let _ = std::fs::write(data_dir.join("nicknames.cbor"), buf);
    }
}

/// Persisted last-seen times (NodeId hex → ms) so "offline > 1 week" survives
/// daemon restarts. Loaded into the presence tracker at startup.
fn load_last_seen(data_dir: &Path) -> Vec<(Id, u64)> {
    let map: HashMap<String, u64> = std::fs::read(data_dir.join("last_seen.cbor"))
        .ok()
        .and_then(|b| ciborium::from_reader(b.as_slice()).ok())
        .unwrap_or_default();
    map.into_iter()
        .filter_map(|(hex, ts)| parse_id(&hex).ok().map(|id| (id, ts)))
        .collect()
}

fn save_last_seen(data_dir: &Path, map: &HashMap<String, u64>) {
    let _ = std::fs::create_dir_all(data_dir);
    let mut buf = Vec::new();
    if ciborium::into_writer(map, &mut buf).is_ok() {
        let _ = std::fs::write(data_dir.join("last_seen.cbor"), buf);
    }
}

fn load_config(data_dir: &Path) -> Result<Option<StoredConfig>> {
    let on_disk: OnDiskConfig = match std::fs::read(config_path(data_dir)) {
        Ok(bytes) => ciborium::from_reader(bytes.as_slice()).context("decode network config")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read network config"),
    };
    // The secret lives in the keystore. If it's gone (e.g. keystore cleared),
    // treat it as "no network" rather than running with a broken config.
    let Some(secret) = crate::secrets::load(data_dir, KEY_NETWORK_SECRET)? else {
        tracing::warn!("network config present but its secret is missing; ignoring it");
        return Ok(None);
    };
    let originator_secret = crate::secrets::load(data_dir, KEY_ORIGINATOR_SECRET)?;
    Ok(Some(StoredConfig {
        name: on_disk.name,
        subnet: on_disk.subnet,
        secret,
        originator_id: on_disk.originator_id,
        originator_secret,
    }))
}

fn save_config(data_dir: &Path, cfg: &StoredConfig) -> Result<()> {
    std::fs::create_dir_all(data_dir).ok();
    let on_disk = OnDiskConfig {
        name: cfg.name.clone(),
        subnet: cfg.subnet,
        originator_id: cfg.originator_id,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&on_disk, &mut buf).context("encode network config")?;
    std::fs::write(config_path(data_dir), buf).context("write network config")?;
    // Secrets go to the OS keystore (file fallback handled inside `secrets`).
    crate::secrets::store(data_dir, KEY_NETWORK_SECRET, &cfg.secret)?;
    match cfg.originator_secret {
        Some(s) => crate::secrets::store(data_dir, KEY_ORIGINATOR_SECRET, &s)?,
        None => crate::secrets::delete(data_dir, KEY_ORIGINATOR_SECRET),
    }
    Ok(())
}

async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let mut buf = Vec::new();
    ciborium::into_writer(msg, &mut buf).context("encode msg")?;
    let len = (buf.len() as u32).to_be_bytes();
    send.write_all(&len).await.context("write len")?;
    send.write_all(&buf).await.context("write body")?;
    Ok(())
}

async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut lb = [0u8; 4];
    recv.read_exact(&mut lb).await.context("read len")?;
    let len = u32::from_be_bytes(lb) as usize;
    if len > (1 << 20) {
        bail!("message too large");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.context("read body")?;
    ciborium::from_reader(buf.as_slice()).context("decode msg")
}
