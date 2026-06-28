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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::admission;
use crate::membership;
use crate::network::{
    decode_recovery_key, encode_recovery_key, generate_originator_key, NetworkSecret, Ticket,
};
use crate::node::IrohNode;
use crate::presence::{Presence, PresenceTracker};
use crate::roster::{now_ms, sign, Config, Id, Op, Roster};
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
    /// The device's actual current OS hostname (source of truth).
    pub hostname: Option<String>,
    /// Optional friendly name the member set for itself.
    pub label: Option<String>,
    pub virtual_ip: Option<String>,
    pub observed_addr: Option<String>,
    pub direct: Option<bool>,
    pub online: bool,
    pub last_seen: u64,
    pub is_self: bool,
    pub is_originator_device: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub name: String,
    pub subnet: String,
    pub frozen: bool,
    pub self_node_id: String,
    pub self_ip: Option<String>,
    /// This device's own friendly label (for prefilling the "set name" UI).
    pub self_label: Option<String>,
    pub is_originator: bool,
    /// Whether the TUN is up so RDP/SSH traffic is actually routed (needs elevation).
    pub routing: bool,
    /// Whether the daemon is currently connected to the network (vs. disconnected
    /// via "Quit", but still holding the config).
    pub online: bool,
    pub members: Vec<MemberView>,
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
    /// This device's member-chosen friendly label (the OS hostname is read live,
    /// never cached, so it always reflects the actual current hostname).
    label: StdRwLock<Option<String>>,
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
    /// Abort handles for network-scoped background tasks (presence receiver, TUN
    /// read loop) so leaving/deleting a network stops them cleanly.
    net_tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Whether we've ever seen ourselves in the roster. Once true, dropping out of
    /// the roster means we were removed → auto-leave (handles remove/delete/rotate).
    was_member: AtomicBool,
    /// Our mesh/join protocol version (normally `admission::PROTOCOL_VERSION`;
    /// overridable in tests to exercise the mismatch path).
    protocol_version: AtomicU32,
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
        let label = load_label(&data_dir);

        let (events, _) = broadcast::channel(64);
        let inner = Arc::new(Inner {
            node,
            device_key,
            my_id,
            label: StdRwLock::new(label),
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
            net_tasks: StdMutex::new(Vec::new()),
            was_member: AtomicBool::new(false),
            protocol_version: AtomicU32::new(admission::PROTOCOL_VERSION),
        });

        // Accept loops for our custom ALPNs.
        spawn_accept_loop(inner.clone(), mesh_rx, handle_mesh_incoming);
        spawn_accept_loop(inner.clone(), join_rx, handle_join_incoming);

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

        // Genesis entry: originator master key vouches its own device in. The IP
        // is assigned deterministically from the NodeId when the roster is folded.
        let genesis = sign(
            secret.network_id(),
            &originator,
            Op::Add {
                node_id: self.inner.my_id,
                hostname: current_hostname(),
                ts: now_ms(),
            },
        );
        publish(&self.inner, &genesis).await?;

        let ticket = Ticket::new(name, subnet, &secret, originator_id, self.inner.node.addr());
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
        activate(&self.inner, cfg.clone()).await?;

        // Dial the bootstrap member on the join ALPN (full address from the ticket).
        let conn = self
            .inner
            .node
            .endpoint
            .connect(ticket.bootstrap.clone(), JOIN_ALPN)
            .await
            .context("dial bootstrap member")?;
        let psk = secret.psk();
        let verified = admission::dial(
            &conn,
            self.inner.my_id,
            &psk,
            self.inner.protocol_version.load(Ordering::SeqCst),
        )
        .await?;
        let _ = self.inner.events.send(EngineEvent::JoinSas {
            sas: verified.sas.iter().map(|s| s.to_string()).collect(),
        });

        // Send our join request and wait for the member's decision.
        let (mut send, mut recv) = conn.open_bi().await.context("open join stream")?;
        write_msg(
            &mut send,
            &JoinRequest {
                hostname: current_hostname(),
            },
        )
        .await?;
        let resp: JoinResponse = read_msg(&mut recv).await.context("read join response")?;
        match resp {
            JoinResponse::Approved => {
                save_config(&self.inner.data_dir, &cfg)?;
                self.inner.state.lock().await.config = Some(cfg);
                // Pull the roster from the member who admitted us. Our virtual IP
                // is derived from our NodeId once we appear in the synced roster;
                // the periodic tick then brings up routing.
                if let Some(doc) = self.inner.state.lock().await.doc.clone() {
                    let _ = doc.start_sync(vec![ticket.bootstrap.clone()]).await;
                }
                let _ = self.inner.events.send(EngineEvent::Changed);
                Ok(())
            }
            JoinResponse::Denied => bail!("the member declined the join request"),
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

    /// Originator-only: remove a member.
    pub async fn remove_member(&self, node_id_hex: &str) -> Result<()> {
        let id = parse_id(node_id_hex)?;
        let originator = self.originator_key().await?;
        let net_id = {
            let st = self.inner.state.lock().await;
            st.config.as_ref().unwrap().secret().network_id()
        };
        let entry = sign(
            net_id,
            &originator,
            Op::Remove {
                node_id: id,
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

        // Genesis self-add in the NEW namespace (IP derived during the fold).
        let genesis = sign(
            secret.network_id(),
            &originator,
            Op::Add {
                node_id: self.inner.my_id,
                hostname: current_hostname(),
                ts: now_ms(),
            },
        );
        publish(&self.inner, &genesis).await?;

        let ticket = Ticket::new(name, subnet, &secret, originator_id, self.inner.node.addr());
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
    /// GUI: "Quit IPN" disconnects (the device goes offline from the pool) but
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

    /// The join ticket for this network (to onboard another device).
    pub async fn ticket(&self) -> Result<String> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        Ok(Ticket::new(
            cfg.name.clone(),
            cfg.subnet(),
            &cfg.secret(),
            cfg.originator_id,
            self.inner.node.addr(),
        )
        .encode())
    }

    /// Set (or clear, with `None`/empty) this device's friendly label. The label
    /// is persisted and broadcast via presence; the OS hostname is never editable.
    pub async fn set_label(&self, label: Option<String>) -> Result<()> {
        let label = label.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        *self.inner.label.write().unwrap() = label.clone();
        save_label(&self.inner.data_dir, label.as_deref());
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Snapshot of the network for display.
    pub async fn status(&self) -> Result<NetworkStatus> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let mut members = Vec::new();
        for (id, m) in st.roster.members() {
            let ps = st.presence.get(id);
            let is_self = *id == self.inner.my_id;
            members.push(MemberView {
                node_id: data_encoding::HEXLOWER.encode(id),
                hostname: if is_self {
                    Some(current_hostname())
                } else {
                    ps.and_then(|p| p.hostname.clone())
                        .or_else(|| Some(m.hostname.clone()))
                },
                label: if is_self {
                    self.inner.label.read().unwrap().clone()
                } else {
                    ps.and_then(|p| p.label.clone())
                },
                virtual_ip: Some(m.virtual_ip.to_string()),
                observed_addr: ps.and_then(|p| p.observed_addr.clone()),
                direct: ps.and_then(|p| p.direct),
                online: is_self || ps.map(|p| p.online).unwrap_or(false),
                last_seen: ps.map(|p| p.last_seen).unwrap_or(0),
                is_self,
                is_originator_device: m.added_by == cfg.originator_id && false, // device==originator-master only at genesis; informational
            });
        }
        members.sort_by(|a, b| b.online.cmp(&a.online).then(a.node_id.cmp(&b.node_id)));
        Ok(NetworkStatus {
            name: cfg.name.clone(),
            subnet: cfg.subnet().to_string(),
            frozen: st.roster.frozen(),
            self_node_id: data_encoding::HEXLOWER.encode(&self.inner.my_id),
            self_ip: st
                .roster
                .member(&self.inner.my_id)
                .map(|m| m.virtual_ip.to_string()),
            self_label: self.inner.label.read().unwrap().clone(),
            is_originator: cfg.originator_secret.is_some(),
            routing: self.inner.tun.read().unwrap().is_some(),
            online: st.doc.is_some(),
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
        let h = tokio::spawn(async move {
            while let Some(ev) = receiver.next().await {
                let Ok(ev) = ev else { continue };
                if let Event::Received(m) = ev {
                    if let Ok(p) = ciborium::from_reader::<Presence, _>(m.content.as_ref()) {
                        if p.verify(&net_id) && p.node_id != ti.my_id {
                            let mut st = ti.state.lock().await;
                            st.presence
                                .record_heartbeat(p.node_id, p.hostname, p.label, p.ts);
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
            .record_connection(id, None, None, false);
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
        let p = Presence::signed(
            cfg.secret().network_id(),
            &inner.device_key,
            current_hostname(),
            inner.label.read().unwrap().clone(),
            now_ms(),
        );
        let mut buf = Vec::new();
        let _ = ciborium::into_writer(&p, &mut buf);
        let _ = sender.broadcast(Bytes::from(buf)).await;
        if !peers.is_empty() {
            let _ = sender.join_peers(peers).await;
        }
    }

    // Refresh observed-address / direct info for live peers.
    let live: Vec<Id> = inner.conns.read().unwrap().keys().copied().collect();
    for peer in live {
        let (addr, direct) = observed(inner, &peer).await;
        let mut st = inner.state.lock().await;
        st.presence.record_connection(peer, addr, direct, true);
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
        match admit_member(&inner, net_id, verified.peer_id, req.hostname).await {
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

/// Write a signed `Add` vouching the joiner in (web-of-trust). The joiner's IP is
/// derived from its NodeId when the roster is folded, so no IP is chosen here.
async fn admit_member(inner: &Arc<Inner>, net_id: Id, peer: Id, hostname: String) -> Result<()> {
    let frozen = {
        let st = inner.state.lock().await;
        st.roster.frozen()
    };
    if frozen {
        bail!("roster is frozen");
    }
    let entry = sign(
        net_id,
        &inner.device_key,
        Op::Add {
            node_id: peer,
            hostname,
            ts: now_ms(),
        },
    );
    publish(inner, &entry).await
}

async fn register_mesh(inner: &Arc<Inner>, peer: Id, conn: Connection) {
    {
        inner.conns.write().unwrap().insert(peer, conn.clone());
        let mut st = inner.state.lock().await;
        st.presence.record_connection(peer, None, None, true);
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
                            // Clamp inbound TCP SYNs too (bounds the other direction).
                            let mut pkt = pkt.to_vec();
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
        st.presence.record_connection(peer, None, None, false);
        drop(st);
        let _ = inner2.events.send(EngineEvent::Changed);
    });
}

/// Bring up the OS TUN once, if we know our virtual IP. Best-effort: if it fails
/// (no elevation, missing wintun.dll, …) we log and keep running without routing,
/// so membership + presence still work. Spawns the outbound read loop on success.
async fn enable_tun(inner: &Arc<Inner>, ip: Ipv4Addr) {
    // Escape hatch for tests/CI (and headless runs where a TUN is undesirable).
    if std::env::var_os("IPN_DISABLE_TUN").is_some() {
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

/// Classify the live path to a peer (direct vs relay) and its observed address.
async fn observed(inner: &Arc<Inner>, peer: &Id) -> (Option<String>, Option<bool>) {
    let Ok(eid) = EndpointId::from_bytes(peer) else {
        return (None, None);
    };
    let Some(info) = inner.node.endpoint.remote_info(eid).await else {
        return (None, None);
    };
    use iroh::endpoint::TransportAddrUsage;
    let mut ip = None;
    let mut relay = false;
    for a in info.addrs() {
        if matches!(a.usage(), TransportAddrUsage::Active) {
            if a.addr().is_ip() {
                ip = Some(format!("{}", a.addr()));
            } else if a.addr().is_relay() {
                relay = true;
            }
        }
    }
    let direct = match (ip.is_some(), relay) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    };
    (ip, direct)
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
        .unwrap_or_else(|| "ipn-device".into())
}

fn label_path(data_dir: &Path) -> PathBuf {
    data_dir.join("label")
}

/// Load this device's friendly label, if set (a plain UTF-8 file).
fn load_label(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(label_path(data_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist (or clear) this device's friendly label. Best-effort.
fn save_label(data_dir: &Path, label: Option<&str>) {
    let _ = std::fs::create_dir_all(data_dir);
    match label {
        Some(l) => {
            let _ = std::fs::write(label_path(data_dir), l);
        }
        None => {
            let _ = std::fs::remove_file(label_path(data_dir));
        }
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
