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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use ed25519_dalek::SigningKey;
use futures_lite::StreamExt;
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, Side};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use iroh_docs::api::Doc;
use iroh_docs::AuthorId;
use iroh_gossip::api::{Event, GossipSender};
use iroh_gossip::proto::TopicId;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify};

use crate::admission;
use crate::membership;
use crate::conntrack::Conntrack;
use crate::network::{
    decode_recovery_key, encode_recovery_key, generate_originator_key, NetworkSecret, Ticket,
};
use crate::node::IrohNode;
use crate::presence::{GossipMsg, Locations, Presence, PresenceTracker};
use crate::roster::{now_ms, sign, Config, Id, InviteCheck, InviteKind, Nonce, Op, Role, Roster};
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

/// How often (ms) to re-seed the roster-doc live-sync gossip swarm as a periodic
/// self-heal, independent of membership changes. A membership change re-seeds
/// *all* members immediately (that's what keeps freshness tight); this periodic
/// pass only needs to keep an already-connected swarm healthy, so it targets
/// **reachable** members (see [`doc_reseed_targets`]). Kept ≤ the ~45s removal-
/// propagation window the `delete_e2e`/`rotate_e2e` tests assert. It was 8s and
/// unfiltered, which re-dialed every *unreachable* member every 8s — each attempt
/// minting a permanent iroh mapped-address entry (n0-computer/iroh#4293) until the
/// daemon memory watchdog restarted the process and dropped every connection. That
/// restart loop was the main cause of the intermittent drops.
const DOC_RESYNC_MS: u64 = 30_000;

/// A peer whose presence heartbeat we've heard within this window counts as
/// "reachable" for the periodic doc self-heal even without a live mesh connection
/// (the roster-doc and mesh use separate iroh protocols, so a member can be gossip-
/// reachable while its mesh link is momentarily down). Re-seeding these is cheap and
/// is what actually keeps the swarm connected; re-seeding *unreachable* members is
/// the churn that feeds iroh#4293.
const PRESENCE_FRESH_MS: u64 = 300_000;

/// Cadence for growing the presence gossip mesh via `join_peers`. Like the doc
/// re-seed this dials members, so it's throttled off the 3s tick; a membership
/// change still triggers it immediately.
const GOSSIP_JOIN_MS: u64 = 60_000;

/// Faster `join_peers` cadence while we have **zero** gossip neighbors — we're
/// isolated and need to (re)join the mesh promptly (e.g. right after startup or a
/// network blip). Once neighbors exist we fall back to [`GOSSIP_JOIN_MS`].
const GOSSIP_JOIN_RETRY_MS: u64 = 15_000;

/// Upper bound on a single mesh dial (`endpoint.connect()` + admission handshake).
/// Without this an unreachable member's `connect()` can stay pending indefinitely;
/// combined with the periodic re-dial that would accumulate iroh connection/path
/// state every tick (see the daemon memory-growth investigation). Generous enough
/// for a slow relay+holepunch path on a healthy network.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

/// Maintenance-loop cadence in [`Pace::Interactive`] — the historical 3s tick that
/// desktop uses always and the Android app uses while it's in the foreground.
const TICK_INTERACTIVE_MS: u64 = 3_000;

/// Maintenance-loop cadence in [`Pace::Background`] — the Android app while it's
/// backgrounded / screen-off. 20× slower: the mesh stays up and a roster-doc change
/// still wakes the loop early (see [`Inner::tick_notify`]), but the per-3s crypto/
/// gossip/redb work that dominated idle battery use stops while nobody's looking.
const TICK_BACKGROUND_MS: u64 = 60_000;

/// Presence-heartbeat floor in [`Pace::Background`]. Well inside every peer's
/// [`PRESENCE_FRESH_MS`] window, and a peer's *online* dot is derived from the live
/// mesh connection (not this heartbeat), so slowing it changes only the last-seen
/// granularity of a phone that later goes offline — not anyone's visibility.
const PRESENCE_BROADCAST_BG_MS: u64 = 60_000;

/// Rebuild the roster from the doc at least this often even absent a live-sync
/// event, so a missed [`iroh_docs::api::Doc::subscribe`] notification can't leave
/// the view stale. Bounds the staleness the event-gated rebuild could otherwise add.
const ROSTER_REBUILD_CATCHALL_MS: u64 = 30_000;

/// Upper bound on the per-peer dial backoff. After consecutive failures the next
/// attempt is delayed `min(DIAL_TIMEOUT · 2^failures, this)` — so an unreachable
/// member is retried ever more sparsely (was a flat ~20s forever) but never dropped.
const DIAL_BACKOFF_MAX_MS: u64 = 300_000;

/// How aggressively the maintenance loop runs. Desktop is always [`Pace::Interactive`];
/// the Android facade drops to [`Pace::Background`] when the app isn't visible
/// (screen off / backgrounded) to cut idle battery use. See [`Engine::set_pace`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pace {
    Interactive,
    Background,
}

/// Per-peer dial-backoff state (see [`DIAL_BACKOFF_MAX_MS`]).
#[derive(Clone, Copy, Default)]
struct BackoffEntry {
    failures: u32,
    /// Earliest wall-clock (ms) a fresh dial to this peer is allowed.
    next_ok_ms: u64,
}

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
    /// This client's **local** free-text note about the member (not shared).
    #[serde(default)]
    pub note: Option<String>,
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
    /// **Android only:** routing needs the platform TUN. Our virtual `ip` is known,
    /// so the app should bring up its `VpnService` with this address/MTU and hand
    /// the resulting fd back via [`Engine::attach_tun_fd`]. Never emitted on desktop
    /// (which opens its own TUN), so desktop clients can ignore it.
    TunSetupRequired { ip: String, mtu: u32 },
    /// **Android only:** routing is going away (went offline / left the network) —
    /// the app should tear down its `VpnService`. Never emitted on desktop.
    TunTeardownRequired,
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
    /// Declined; carries an optional human-readable reason (e.g. a used/expired
    /// invite) so the joiner can show something better than a generic failure.
    Denied(Option<String>),
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
    /// This client's **local** free-text notes about other members, keyed by
    /// NodeId hex. Never broadcast — purely local, like nicknames.
    notes: StdRwLock<HashMap<String, String>>,
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
    /// Our roster-assigned virtual IP, once known. On Android the app reads this
    /// (via [`Engine::assigned_ip`]) to build the `VpnService` before consent; the
    /// engine can't open the TUN itself there.
    assigned_ip: StdRwLock<Option<Ipv4Addr>>,
    /// Members we've currently seeded the roster-doc live-sync gossip swarm with,
    /// so we only re-`start_sync` when the set changes (plus a periodic self-heal).
    /// Without keeping this swarm seeded with *all* members, a later Add/Remove/role
    /// change — and the audit log derived from them — only reaches members with a
    /// healthy direct link and is otherwise missed until a restart re-resumes sync.
    doc_sync_set: StdMutex<std::collections::BTreeSet<Id>>,
    /// Last time (ms) we (re)seeded the roster-doc live-sync swarm.
    last_doc_sync: AtomicU64,
    /// Last time (ms) we grew the presence gossip mesh via `join_peers`. Throttled
    /// off the 3s tick (see [`GOSSIP_JOIN_MS`]) because each call dials members —
    /// the same unbounded-address-cache churn the doc re-seed throttle avoids.
    last_gossip_join: AtomicU64,
    /// Live count of presence-gossip neighbors, tracked from the swarm's
    /// `NeighborUp`/`NeighborDown` events. Zero means we're isolated, which switches
    /// `join_peers` to the faster [`GOSSIP_JOIN_RETRY_MS`] cadence.
    gossip_neighbors: AtomicUsize,
    /// Members with a mesh dial currently in flight, so the periodic tick doesn't
    /// stack a fresh `connect()` on top of one already in progress. Without this,
    /// an offline member accrued a new connection attempt (and its iroh path/QUIC
    /// state) every tick, which never got reclaimed — the daemon memory leak. Held
    /// in an `Arc` so the fan-out helper can be exercised without a live node.
    dialing: DialingSet,
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
    /// Inbound packets dropped by the one-way block since the last tick (logged
    /// per-interval so the block is observable in the daemon log).
    blocked_inbound: AtomicU64,
    /// Abort handles for network-scoped background tasks (presence receiver, TUN
    /// read loop) so leaving/deleting a network stops them cleanly.
    net_tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Whether we've ever seen ourselves in the roster. Once true, dropping out of
    /// the roster means we were removed → auto-leave (handles remove/delete/rotate).
    was_member: AtomicBool,
    /// Our mesh/join protocol version (normally `admission::PROTOCOL_VERSION`;
    /// overridable in tests to exercise the mismatch path).
    protocol_version: AtomicU32,
    /// User-configured custom relay servers (see [`crate::relays`]). The live
    /// endpoint relay map is kept in sync at runtime, so edits apply without a
    /// daemon restart. This is the source of truth the moment the user saves —
    /// pushing it into the endpoint happens behind it, in `apply_relay_map`.
    relay_settings: StdRwLock<crate::relays::RelaySettings>,
    /// How far the last relay-settings change got in reaching the live endpoint.
    relay_apply: StdRwLock<crate::relays::RelayApply>,
    /// Serializes appliers so two rapid edits can't interleave their inserts and
    /// removes into the endpoint's relay map.
    relay_apply_lock: Mutex<()>,
    /// Bumped on every settings change; an applier that finds it moved on was
    /// superseded while it waited for the lock, and drops out.
    relay_apply_gen: AtomicU64,
    /// Last time (ms) we flushed the persisted last-seen map (throttle).
    last_seen_saved: AtomicU64,
    /// Monotonic tick counter, so every 10th tick (~30s) still emits a
    /// `Changed` even when nothing marked the tick dirty (see `tick`).
    tick_seq: AtomicU64,
    /// Maintenance-loop pace: `0` = [`Pace::Interactive`] (default; desktop always),
    /// `1` = [`Pace::Background`]. Switched by [`Engine::set_pace`] from the Android
    /// facade on screen/visibility changes; read by the loop to pick its sleep.
    pace: AtomicU8,
    /// Wakes the maintenance loop before its sleep elapses — on a pace change, a
    /// network-change hint, or a roster-doc live-sync event — so a slow Background
    /// tick still reacts to real changes within a tick instead of up to 60s later.
    tick_notify: Notify,
    /// Set by the roster-doc live-sync watcher (see `activate`); consumed by `tick`
    /// to decide whether to rebuild the roster this pass. Makes the per-tick redb+
    /// blob roster rebuild event-driven instead of unconditional (a real idle cost
    /// on every platform), bounded by [`ROSTER_REBUILD_CATCHALL_MS`].
    docs_dirty: AtomicBool,
    /// Last wall-clock (ms) `tick` rebuilt the roster from the doc (catch-all clock).
    last_roster_rebuild: AtomicU64,
    /// Last wall-clock (ms) we broadcast a presence heartbeat (Background throttle).
    last_presence_broadcast: AtomicU64,
    /// One-shot: a platform network-change hint ([`Engine::network_changed`]) asks
    /// the next tick to bypass the reachability/throttle gates once and re-seed +
    /// re-dial *every* member (recovery after another VPN released the network).
    force_recover: AtomicBool,
    /// Per-peer dial-backoff schedule (see [`BackoffEntry`], [`DIAL_BACKOFF_MAX_MS`]).
    dial_backoff: StdMutex<HashMap<Id, BackoffEntry>>,
    /// Geolocation DB, loaded only on the originator (it resolves + propagates).
    /// Desktop-only — the geo stack isn't shipped on Android.
    #[cfg(not(target_os = "android"))]
    geo: StdRwLock<Option<crate::geo::GeoDb>>,
    /// Guards against launching multiple concurrent geo-DB downloads.
    #[cfg(not(target_os = "android"))]
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
        let notes = load_notes(&data_dir);
        let prefs = load_device_prefs(&data_dir);
        // `IrohNode::spawn_with` already applied these to the endpoint at bind.
        let relay_settings = crate::relays::load_relay_settings(&data_dir);

        let (events, _) = broadcast::channel(64);
        let inner = Arc::new(Inner {
            node,
            device_key,
            my_id,
            nicknames: StdRwLock::new(nicknames),
            notes: StdRwLock::new(notes),
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
            assigned_ip: StdRwLock::new(None),
            doc_sync_set: StdMutex::new(std::collections::BTreeSet::new()),
            last_doc_sync: AtomicU64::new(0),
            last_gossip_join: AtomicU64::new(0),
            gossip_neighbors: AtomicUsize::new(0),
            dialing: DialingSet::default(),
            remote_access_disabled: AtomicBool::new(prefs.remote_access_disabled),
            hidden: AtomicBool::new(prefs.hidden),
            conntrack: Conntrack::default(),
            coarse_now: AtomicU64::new(0),
            blocked_inbound: AtomicU64::new(0),
            net_tasks: StdMutex::new(Vec::new()),
            was_member: AtomicBool::new(false),
            protocol_version: AtomicU32::new(admission::PROTOCOL_VERSION),
            last_seen_saved: AtomicU64::new(0),
            tick_seq: AtomicU64::new(0),
            pace: AtomicU8::new(0),
            tick_notify: Notify::new(),
            docs_dirty: AtomicBool::new(false),
            last_roster_rebuild: AtomicU64::new(0),
            last_presence_broadcast: AtomicU64::new(0),
            force_recover: AtomicBool::new(false),
            dial_backoff: StdMutex::new(HashMap::new()),
            relay_settings: StdRwLock::new(relay_settings),
            relay_apply: StdRwLock::new(crate::relays::RelayApply::Applied),
            relay_apply_lock: Mutex::new(()),
            relay_apply_gen: AtomicU64::new(0),
            #[cfg(not(target_os = "android"))]
            geo: StdRwLock::new(None),
            #[cfg(not(target_os = "android"))]
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
                    // Sleep for the current pace, but wake early on a pace change,
                    // a network-change hint, or a roster-doc live-sync event.
                    let ms = match inner.pace.load(Ordering::Relaxed) {
                        0 => TICK_INTERACTIVE_MS,
                        _ => TICK_BACKGROUND_MS,
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                        _ = inner.tick_notify.notified() => {}
                    }
                }
            });
        }

        Ok(Engine { inner })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// Switch the maintenance-loop pace. Android drops to [`Pace::Background`] when
    /// the app isn't visible (screen off / backgrounded) to cut idle battery use;
    /// desktop never calls this and stays [`Pace::Interactive`]. Wakes the loop
    /// immediately so returning to the foreground refreshes state within a tick.
    pub fn set_pace(&self, pace: Pace) {
        let v = match pace {
            Pace::Interactive => 0,
            Pace::Background => 1,
        };
        if self.inner.pace.swap(v, Ordering::Relaxed) != v {
            tracing::info!("maintenance pace -> {pace:?}");
        }
        self.inner.tick_notify.notify_one();
    }

    /// Platform hint that connectivity changed — the **only** such signal on Android,
    /// where iroh's own network monitor is a no-op (netlink is restricted), so an
    /// endpoint bound before a network switch otherwise keeps stale sockets/paths/
    /// relay connections forever (the "can't see peers until I toggle the VPN" bug).
    ///
    /// Hands the hint to iroh (which rebinds its UDP sockets, re-checks the relay
    /// connection, resets the DNS resolver, and re-runs net-report), then forces a
    /// one-shot recovery burst — re-seed the roster doc to **all** members, re-join
    /// the gossip mesh, and clear per-peer dial backoff — in case iroh's own
    /// interface-state compare swallowed the hint. Safe to call spuriously.
    pub async fn network_changed(&self) {
        let inner = &self.inner;
        tracing::info!("network-change hint: rebinding endpoint + recovery burst");
        inner.node.endpoint.network_change().await;
        inner.force_recover.store(true, Ordering::SeqCst);
        // Make the throttled self-heals due immediately; the tick's `force` path then
        // targets everyone rather than only presence-fresh members.
        inner.last_doc_sync.store(0, Ordering::SeqCst);
        inner.last_gossip_join.store(0, Ordering::SeqCst);
        inner.dial_backoff.lock().unwrap().clear();
        inner.tick_notify.notify_one();
    }

    pub fn self_node_id_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.inner.my_id)
    }

    /// Our roster-assigned virtual IP (`10.99.0.x`), once known — `None` before a
    /// network is active. On Android the app reads this to build its `VpnService`
    /// after receiving [`EngineEvent::TunSetupRequired`].
    pub fn assigned_ip(&self) -> Option<String> {
        self.inner
            .assigned_ip
            .read()
            .unwrap()
            .map(|ip| ip.to_string())
    }

    /// **Android only:** adopt the TUN file descriptor produced by the app's
    /// `VpnService` (`ParcelFileDescriptor.detachFd()`) and start routing. Takes
    /// ownership of `fd` (closed when routing tears down). Idempotent: replaces any
    /// existing device and pump. This is the Android equivalent of the desktop's
    /// internal `RealTun::open`.
    #[cfg(target_os = "android")]
    pub fn attach_tun_fd(&self, fd: i32) -> Result<()> {
        // SAFETY: contract documented on the facade — `fd` is an owned, open fd
        // surrendered by Kotlin via detachFd() and never touched again there.
        let tun = unsafe { RealTun::from_fd(fd, TUN_MTU) }.context("adopt VpnService tun fd")?;
        let tun = Arc::new(tun);
        // Mark attempted so the maintenance tick won't try to re-enable.
        self.inner.tun_attempted.store(true, Ordering::SeqCst);
        *self.inner.tun.write().unwrap() = Some(tun.clone());
        tracing::info!("routing enabled: adopted VpnService tun fd (mtu {TUN_MTU})");
        spawn_tun_pump(&self.inner, tun);
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// **Android only:** drop the TUN (VPN revoked / stopping) and stop the pump.
    /// Resets the attempt latch so a later re-consent can re-attach.
    #[cfg(target_os = "android")]
    pub fn detach_tun(&self) {
        *self.inner.tun.write().unwrap() = None; // drop closes the fd
        self.inner.tun_attempted.store(false, Ordering::SeqCst);
        let _ = self.inner.events.send(EngineEvent::Changed);
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

        // Fold our own genesis + invite into the in-memory roster before handing back
        // the ticket, so we honor the invite immediately instead of rejecting a joiner
        // who connects before the next maintenance tick rebuilds the roster.
        refresh_roster(&self.inner).await;

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

        // As in `create_network`: fold the new namespace's genesis + invite into the
        // in-memory roster now, so a ticket holder joining the rotated network isn't
        // rejected until the next tick.
        refresh_roster(&self.inner).await;

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
        tracing::info!(
            "remote access {}",
            if disabled { "DISABLED — inbound blocked" } else { "enabled" }
        );
        self.persist_prefs();
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Toggle whether this device hides itself from the member list. Hiding
    /// implies the inbound block (the effective block is `disabled || hidden`).
    pub async fn set_hidden(&self, hidden: bool) -> Result<()> {
        self.inner.hidden.store(hidden, Ordering::Relaxed);
        tracing::info!(
            "device {} member list",
            if hidden { "HIDDEN from (inbound blocked)" } else { "visible in" }
        );
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

    /// This device's custom relay configuration (empty = iroh defaults).
    pub fn relay_settings(&self) -> crate::relays::RelaySettings {
        self.inner.relay_settings.read().unwrap().clone()
    }

    /// The relays the live endpoint currently has a connection to, and whether
    /// each is connected. This is the only view iroh exposes into the running
    /// relay map, and it is *not* the whole map: an endpoint homes on a single
    /// relay (the lowest-latency one that answers) and that is the only one it
    /// advertises to peers — see the module docs on [`crate::relays`].
    pub fn relay_connections(&self) -> Vec<(iroh::RelayUrl, bool)> {
        use iroh::Watcher as _;
        self.inner
            .node
            .endpoint
            .home_relay_status()
            .get()
            .iter()
            .map(|s| (s.url().clone(), s.is_connected()))
            .collect()
    }

    /// The relay configuration **and** how far it got in reaching the live
    /// endpoint, so callers can report the truth rather than assume success.
    pub fn relay_status(&self) -> crate::relays::RelayStatus {
        crate::relays::RelayStatus {
            settings: self.inner.relay_settings.read().unwrap().clone(),
            apply: self.inner.relay_apply.read().unwrap().clone(),
        }
    }

    /// Replace the custom relay configuration: validate, persist, and make it
    /// this device's settings — then return. Pushing the new map into the
    /// running endpoint happens in a background task, and its progress shows up
    /// as [`RelayApply`](crate::relays::RelayApply) on [`relay_status`].
    ///
    /// It has to be that way round. `Endpoint::insert_relay`/`remove_relay` look
    /// like setters but each awaits iroh's bounded socket-actor channel, and
    /// that actor can be blocked *indefinitely* by an unrelated peer — see
    /// [`apply_relay_map`]. Doing them inline made this call hang for tens of
    /// minutes on a live mesh, and because the in-memory settings were swapped
    /// *after* the endpoint work, a hung call left disk, path selector, relay
    /// map and reported settings all disagreeing: the CLI blocked forever while
    /// `relay show` truthfully reported the old value.
    ///
    /// [`relay_status`]: Self::relay_status
    pub async fn set_relay_settings(&self, settings: crate::relays::RelaySettings) -> Result<()> {
        use crate::relays;

        // Validate fully before any side effect: a bad URL or token fails here
        // and changes nothing.
        let desired = settings.desired_relay_configs()?;
        let custom_urls: std::collections::BTreeSet<iroh::RelayUrl> =
            settings.urls()?.into_iter().collect();

        relays::save_relay_settings(&self.inner.data_dir, &settings)?;

        // Everything the *user* can observe flips here, atomically and without
        // awaiting anything: the selector, the in-memory settings, the reported
        // apply state.
        self.inner.node.preferred_relays.set(custom_urls.clone());
        let old = std::mem::replace(
            &mut *self.inner.relay_settings.write().unwrap(),
            settings.clone(),
        );
        *self.inner.relay_apply.write().unwrap() = relays::RelayApply::Pending;
        let generation = self.inner.relay_apply_gen.fetch_add(1, Ordering::SeqCst) + 1;

        // Anything we may have put in the map before and no longer want. The
        // public defaults are always candidates for removal: under `Only` they
        // must go, under `Preferred` they're in `desired` and so survive the
        // difference below.
        let mut stale: std::collections::BTreeSet<iroh::RelayUrl> = relays::default_relay_configs()
            .iter()
            .map(|c| c.url.clone())
            .collect();
        stale.extend(old.urls().unwrap_or_default());
        let desired_urls: std::collections::BTreeSet<iroh::RelayUrl> =
            desired.iter().map(|c| c.url.clone()).collect();
        let stale: Vec<iroh::RelayUrl> = stale.difference(&desired_urls).cloned().collect();

        tracing::info!(
            "relay settings saved: {} custom relay(s), mode {:?} — endpoint map: {} relay(s), {} to remove",
            custom_urls.len(),
            settings.mode,
            desired.len(),
            stale.len(),
        );

        let inner = self.inner.clone();
        tokio::spawn(async move { apply_relay_map(inner, generation, desired, stale).await });

        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
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
        out.sort_by_key(|e| std::cmp::Reverse(e.ts));
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

    /// Set (or clear, with `None`/blank) this client's **local** free-text note
    /// for a member. Stored locally and never broadcast.
    pub async fn set_note(&self, node_id_hex: &str, note: Option<String>) -> Result<()> {
        let _ = parse_id(node_id_hex)?; // validate it's a real NodeId hex
        // Keep the note verbatim (it may span lines); only a fully-blank note clears it.
        let note = note.filter(|s| !s.trim().is_empty());
        {
            let mut map = self.inner.notes.write().unwrap();
            match note {
                Some(n) => {
                    map.insert(node_id_hex.to_string(), n);
                }
                None => {
                    map.remove(node_id_hex);
                }
            }
            save_notes(&self.inner.data_dir, &map);
        }
        let _ = self.inner.events.send(EngineEvent::Changed);
        Ok(())
    }

    /// Snapshot of the network for display.
    pub async fn status(&self) -> Result<NetworkStatus> {
        let st = self.inner.state.lock().await;
        let cfg = st.config.as_ref().context("no network")?;
        let nicks = self.inner.nicknames.read().unwrap();
        let notes = self.inner.notes.read().unwrap();
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
                // Local free-text note this client set for the member (never shared).
                note: notes.get(&node_hex).cloned(),
                virtual_ip: Some(m.virtual_ip.to_string()),
                local_ip,
                public_ip,
                location: ps.and_then(|p| p.location.clone()),
                observed_addr: ps.and_then(|p| p.observed_addr.clone()),
                direct: ps.and_then(|p| p.direct),
                online: is_self || ps.map(|p| p.online).unwrap_or(false),
                last_seen: ps.map(|p| p.last_seen).unwrap_or(0),
                is_self,
                is_originator_device: false, // device==originator-master only at genesis; informational (not yet derived)
                role,
                access_disabled,
                hidden,
                node_id: node_hex,
            });
        }
        drop(nicks);
        drop(notes);
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
                // Track swarm membership so the tick can back off `join_peers` once
                // we have neighbors (and speed it up again when we're isolated).
                let m = match ev {
                    Event::Received(m) => m,
                    Event::NeighborUp(_) => {
                        ti.gossip_neighbors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    Event::NeighborDown(_) => {
                        // saturating: never wrap past zero on a spurious/duplicate down.
                        let _ = ti.gossip_neighbors.fetch_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |n| Some(n.saturating_sub(1)),
                        );
                        continue;
                    }
                    Event::Lagged => continue,
                };
                let Ok(msg) = ciborium::from_reader::<GossipMsg, _>(m.content.as_ref()) else {
                    continue;
                };
                match msg {
                    GossipMsg::Presence(p) => {
                        if p.verify(&net_id) && p.node_id != ti.my_id {
                            // A live heartbeat proves the peer is reachable. If it was
                            // in dial backoff, clear it and wake the tick to redial
                            // now instead of waiting out the window. This only fires
                            // for a peer we're *not* connected to (a connected peer
                            // has no backoff entry), so it doesn't defeat the slow
                            // Background cadence in steady state.
                            let was_backed_off =
                                ti.dial_backoff.lock().unwrap().remove(&p.node_id).is_some();
                            let mut st = ti.state.lock().await;
                            let changed = st.presence.record_heartbeat(
                                p.node_id,
                                p.hostname,
                                p.public_ip,
                                p.remote_access_disabled,
                                p.hidden,
                                p.ts,
                            );
                            drop(st);
                            if was_backed_off {
                                ti.tick_notify.notify_one();
                            }
                            // Routine heartbeats only bump last_seen; emitting for
                            // each was N events per 3s — the churn that kept the
                            // GUI re-rendering. Only user-visible changes push.
                            if changed {
                                let _ = ti.events.send(EngineEvent::Changed);
                            }
                        }
                    }
                    GossipMsg::Locations(loc) => {
                        // Trust only the originator's signed location assertions.
                        if loc.verify(&net_id, &originator_id) {
                            let mut st = ti.state.lock().await;
                            let mut changed = false;
                            for (id, location) in loc.entries {
                                changed |= st.presence.set_location(id, Some(location));
                            }
                            drop(st);
                            if changed {
                                let _ = ti.events.send(EngineEvent::Changed);
                            }
                        }
                    }
                }
            }
        });
        inner.net_tasks.lock().unwrap().push(h.abort_handle());
    }

    // Roster-doc live-sync watcher: mark the roster dirty and wake the maintenance
    // tick whenever the document changes, so the per-tick redb+blob roster rebuild
    // is event-driven rather than unconditional (a real idle cost on every
    // platform). A failed subscribe degrades to the catch-all interval, so it's
    // non-fatal — never break going online over it.
    match doc.subscribe().await {
        Ok(mut events) => {
            let ti = inner.clone();
            let h = tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    let Ok(ev) = ev else { continue };
                    use iroh_docs::engine::LiveEvent;
                    if matches!(
                        ev,
                        LiveEvent::InsertLocal { .. }
                            | LiveEvent::InsertRemote { .. }
                            | LiveEvent::ContentReady { .. }
                            | LiveEvent::SyncFinished(_)
                    ) {
                        ti.docs_dirty.store(true, Ordering::SeqCst);
                        ti.tick_notify.notify_one();
                    }
                }
            });
            inner.net_tasks.lock().unwrap().push(h.abort_handle());
        }
        Err(e) => tracing::warn!("roster-doc live-sync subscribe failed: {e:#}"),
    }

    // Force the first post-activate tick to rebuild the roster immediately: the
    // catch-all clock would otherwise let the just-cleared roster read empty for up
    // to ROSTER_REBUILD_CATCHALL_MS after reconnecting.
    inner.last_roster_rebuild.store(0, Ordering::SeqCst);
    inner.docs_dirty.store(true, Ordering::SeqCst);

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
    *inner.assigned_ip.write().unwrap() = None;
    inner.doc_sync_set.lock().unwrap().clear();
    inner.last_doc_sync.store(0, Ordering::SeqCst);
    inner.last_gossip_join.store(0, Ordering::SeqCst);
    inner.gossip_neighbors.store(0, Ordering::SeqCst);
    inner.dial_backoff.lock().unwrap().clear();
    inner.docs_dirty.store(false, Ordering::SeqCst);
    inner.force_recover.store(false, Ordering::SeqCst);
    // Android: ask the app to tear down its VpnService (desktop opened its own TUN,
    // which the `tun.write() = None` above already dropped).
    #[cfg(target_os = "android")]
    let _ = inner.events.send(EngineEvent::TunTeardownRequired);
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
    let dropped = inner.blocked_inbound.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        tracing::info!("one-way block: dropped {dropped} unsolicited inbound packet(s)");
    }

    let (doc, cfg) = {
        let st = inner.state.lock().await;
        match (st.doc.clone(), st.config.clone()) {
            (Some(d), Some(c)) => (d, c),
            _ => return Ok(()),
        }
    };

    // Consume the one-shot network-change recovery flag for this pass.
    let force = inner.force_recover.swap(false, Ordering::SeqCst);

    // Rebuild the roster from the doc only when something changed — a live-sync
    // event set `docs_dirty`, a recovery burst asked for it, or the catch-all
    // interval elapsed. The full redb+blob rebuild every 3s was a real idle cost on
    // every platform; otherwise reuse the cached roster (identical data). The
    // forwarding table derives only from the roster, so it's rebuilt only alongside.
    let rebuild = inner.docs_dirty.swap(false, Ordering::SeqCst)
        || force
        || now_coarse.saturating_sub(inner.last_roster_rebuild.load(Ordering::Relaxed))
            > ROSTER_REBUILD_CATCHALL_MS;
    let roster = if rebuild {
        inner.last_roster_rebuild.store(now_coarse, Ordering::Relaxed);
        membership::build_roster(&cfg.roster_cfg(), &doc, inner.node.blobs.blobs()).await?
    } else {
        inner.state.lock().await.roster.clone()
    };

    // Whether this tick changed anything the UI displays. Only then is a
    // `Changed` event emitted at the end (plus a slow unconditional heartbeat
    // for the few live-read status fields nothing marks dirty, e.g. home relay)
    // — an idle tick used to emit unconditionally, which with the per-heartbeat
    // events kept the GUI re-rendering constantly.
    let mut dirty = false;

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

    if rebuild {
        *inner.routes.write().unwrap() = RouteTable::from_roster(&roster);
    }

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
        dirty |= inner
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
        // Membership / roles / name / frozen / invites — everything status()
        // reads from the roster.
        dirty |= st.roster != roster;
        st.roster = roster.clone();
    }

    // Dial missing members (off-lock). `spawn_dials` skips peers already being
    // dialed and bounds each attempt; on top of that, `dial_backoff_filter` spaces
    // out retries to a *persistently* unreachable member (previously a flat ~20s
    // forever), which both saves battery and further shrinks the iroh#4293 churn.
    let to_dial = dial_backoff_filter(
        to_dial,
        &inner.dial_backoff.lock().unwrap(),
        now_coarse,
    );
    let psk = cfg.secret().psk();
    let dial_inner = inner.clone();
    let outcome_inner = inner.clone();
    spawn_dials(
        &inner.dialing,
        to_dial,
        move |peer| {
            let inner = dial_inner.clone();
            async move { dial_member(&inner, peer, psk).await }
        },
        move |peer, success| {
            if success {
                reset_dial_backoff(&outcome_inner.dial_backoff, &peer);
            } else {
                record_dial_failure(&outcome_inner.dial_backoff, peer, now_ms());
            }
        },
    );

    // Keep the roster-doc's live-sync gossip swarm seeded so a later Add/Remove/role
    // change (and the activity log derived from them, and a device's own removal)
    // reaches everyone — iroh-docs only broadcasts document updates within this
    // swarm, so a member that drifted out of it would miss updates until a restart.
    //
    // A membership **change** re-seeds *all* members at once (that's what keeps
    // propagation tight). The periodic self-heal only re-seeds members we believe
    // are **reachable** (a live mesh conn or a recent presence heartbeat): re-dialing
    // *unreachable* members on a timer is what grew iroh's mapped-address cache
    // (iroh#4293) and drove the watchdog restart loop — the main cause of the drops.
    // A removed device still hears its ex-peers' heartbeats, so it stays in their
    // reachable set long enough to pull the Remove entry well inside the e2e windows.
    //
    // A member-set change also forces the throttled gossip `join_peers` below.
    let member_set_changed;
    {
        let member_ids: std::collections::BTreeSet<Id> = roster
            .members()
            .map(|(id, _)| *id)
            .filter(|id| *id != inner.my_id)
            .collect();
        let now = now_ms();
        let changed = {
            let mut last = inner.doc_sync_set.lock().unwrap();
            if *last != member_ids {
                *last = member_ids.clone();
                true
            } else {
                false
            }
        };
        member_set_changed = changed;
        let due = now.saturating_sub(inner.last_doc_sync.load(Ordering::Relaxed)) > DOC_RESYNC_MS;
        // Snapshot which members have a fresh presence heartbeat (reachable even
        // without a live mesh conn); done under the state lock, off any await.
        let fresh: std::collections::HashSet<Id> = {
            let st = inner.state.lock().await;
            member_ids
                .iter()
                .copied()
                .filter(|id| {
                    st.presence
                        .get(id)
                        .is_some_and(|p| now.saturating_sub(p.last_seen) <= PRESENCE_FRESH_MS)
                })
                .collect()
        };
        // `force` (a network-change recovery burst) re-seeds *all* members once,
        // bypassing the reachable-only filter: after minutes behind another VPN no
        // peer is presence-fresh, so the ordinary self-heal would target nothing.
        // A single burst, not a cadence change, so it keeps the iroh#4293 guarantees.
        if let Some(targets) = doc_reseed_targets(
            &member_ids,
            &connected,
            |id| fresh.contains(id),
            changed || force,
            due,
        ) {
            inner.last_doc_sync.store(now, Ordering::Relaxed);
            let addrs: Vec<EndpointAddr> =
                targets.iter().filter_map(|id| bootstrap_addr(id).ok()).collect();
            let doc = doc.clone();
            tokio::spawn(async move {
                if let Err(e) = doc.start_sync(addrs).await {
                    tracing::debug!("roster-doc re-sync failed: {e:#}");
                }
            });
        }
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
        // Interactive pace broadcasts a heartbeat every tick (3s); Background
        // throttles to PRESENCE_BROADCAST_BG_MS to stop per-3s crypto+radio while
        // the app is backgrounded. A peer's online dot is derived from the live mesh
        // connection, not this heartbeat, so slowing it doesn't change visibility.
        let background = inner.pace.load(Ordering::Relaxed) != 0;
        let due_presence = !background
            || now_coarse.saturating_sub(inner.last_presence_broadcast.load(Ordering::Relaxed))
                >= PRESENCE_BROADCAST_BG_MS;
        if due_presence {
            inner.last_presence_broadcast.store(now_coarse, Ordering::Relaxed);
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
        }

        // Originator only: resolve each member's public IP to a location and
        // propagate a signed map so members can show it without the DB. (Not on
        // Android — the geo DB stack isn't shipped there; see `crate::geo`.)
        #[cfg(not(target_os = "android"))]
        if let Some(orig_secret) = cfg.originator_secret.as_ref() {
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
                let orig = SigningKey::from_bytes(orig_secret);
                let loc =
                    Locations::signed(cfg.secret().network_id(), &orig, entries.clone(), now_ms());
                let mut lbuf = Vec::new();
                let _ = ciborium::into_writer(&GossipMsg::Locations(loc), &mut lbuf);
                let _ = sender.broadcast(Bytes::from(lbuf)).await;
                // Apply to our own view (gossip doesn't loop back to us).
                let mut st = inner.state.lock().await;
                for (id, l) in entries {
                    dirty |= st.presence.set_location(id, Some(l));
                }
            }
        }

        // Growing the gossip mesh dials each peer, so throttle it like the doc
        // re-seed: on a member-set change, or on a cadence that's faster while we're
        // isolated (no neighbors) and slow once the mesh is healthy. The 3s presence
        // broadcast above still runs every tick — it only reaches current neighbors
        // and never dials, so it isn't part of the churn.
        if !peers.is_empty() {
            let now = now_ms();
            let cadence = if inner.gossip_neighbors.load(Ordering::Relaxed) == 0 {
                GOSSIP_JOIN_RETRY_MS
            } else {
                GOSSIP_JOIN_MS
            };
            let due =
                now.saturating_sub(inner.last_gossip_join.load(Ordering::Relaxed)) > cadence;
            if member_set_changed || due {
                inner.last_gossip_join.store(now, Ordering::Relaxed);
                let _ = sender.join_peers(peers).await;
            }
        }
    }

    // Originator: keep the geo DB present + fresh (downloads in the background).
    #[cfg(not(target_os = "android"))]
    ensure_geo(inner).await;

    // Refresh observed-address / direct info for live peers.
    let live: Vec<Id> = inner.conns.read().unwrap().keys().copied().collect();
    for peer in live {
        let ci = conn_info(inner, &peer).await;
        let mut st = inner.state.lock().await;
        dirty |= st
            .presence
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

    // Emit on real change, plus a slow (~30s) unconditional heartbeat: status()
    // reads a few values live that no dirty source covers (home relay from
    // node.addr(), this device's own local/public IPs), so a pure dirty flag
    // would let those go stale in the UI forever.
    let seq = inner.tick_seq.fetch_add(1, Ordering::Relaxed);
    if dirty || seq % 10 == 0 {
        let _ = inner.events.send(EngineEvent::Changed);
    }
    Ok(())
}

/// The set of peers with a mesh dial currently in flight (see [`Inner::dialing`]).
/// Shared, so [`spawn_dials`] and [`DialSlot`] depend only on this — the dedup +
/// timeout invariant is unit-testable without constructing a whole [`Inner`].
type DialingSet = Arc<StdMutex<std::collections::HashSet<Id>>>;

/// Frees a peer's in-flight-dial slot when dropped, so a dial that fails, times
/// out, or panics can't leave the peer permanently un-redialable.
struct DialSlot {
    dialing: DialingSet,
    peer: Id,
}

impl Drop for DialSlot {
    fn drop(&mut self) {
        self.dialing.lock().unwrap().remove(&self.peer);
    }
}

/// Spawn a bounded, de-duplicated mesh dial for each peer in `to_dial`.
///
/// This is the fix for the daemon memory leak: the periodic tick calls it every
/// few seconds with every member we aren't connected to, so without a guard an
/// unreachable member spawned a brand-new `connect()` each time and its iroh
/// connection/path state piled up forever. Here a peer already in `dialing` is
/// skipped, and each attempt is wrapped in [`DIAL_TIMEOUT`] so a `connect()` that
/// never resolves can't pin resources; the [`DialSlot`] guard frees the slot on
/// every exit path (success, error, timeout, or panic) so retries still happen.
fn spawn_dials<F, Fut, G>(dialing: &DialingSet, to_dial: Vec<Id>, dial: F, on_outcome: G)
where
    F: Fn(Id) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    G: Fn(Id, bool) + Clone + Send + 'static,
{
    for peer in to_dial {
        // `insert` returns false if a dial to this peer is already in flight.
        if !dialing.lock().unwrap().insert(peer) {
            continue;
        }
        let dialing = dialing.clone();
        let dial = dial.clone();
        let on_outcome = on_outcome.clone();
        tokio::spawn(async move {
            let _slot = DialSlot { dialing, peer };
            let success = match tokio::time::timeout(DIAL_TIMEOUT, dial(peer)).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    tracing::debug!("dial {} failed: {e:#}", short(&peer));
                    false
                }
                Err(_) => {
                    tracing::debug!(
                        "dial {} timed out after {}s",
                        short(&peer),
                        DIAL_TIMEOUT.as_secs()
                    );
                    false
                }
            };
            // Feeds the per-peer backoff (see `dial_backoff_filter`): success clears
            // it, failure/timeout extends it.
            on_outcome(peer, success);
        });
    }
}

/// Filter `to_dial` down to peers whose backoff window has elapsed. Peers with no
/// backoff entry (never failed, or reset by a success/heartbeat) always pass. Pure
/// (no clock, no I/O) so the schedule is unit-tested directly.
fn dial_backoff_filter(
    to_dial: Vec<Id>,
    backoff: &HashMap<Id, BackoffEntry>,
    now: u64,
) -> Vec<Id> {
    to_dial
        .into_iter()
        .filter(|id| backoff.get(id).map_or(true, |e| now >= e.next_ok_ms))
        .collect()
}

/// Record a failed dial to `peer`, extending its backoff window to
/// `min(DIAL_TIMEOUT · 2^failures, DIAL_BACKOFF_MAX_MS)` from `now`.
fn record_dial_failure(backoff: &StdMutex<HashMap<Id, BackoffEntry>>, peer: Id, now: u64) {
    let mut map = backoff.lock().unwrap();
    let e = map.entry(peer).or_default();
    e.failures = e.failures.saturating_add(1);
    let base = DIAL_TIMEOUT.as_millis() as u64;
    // Cap the shift well under 64 to avoid overflow; the value saturates at the max
    // long before that anyway.
    let delay = base
        .saturating_mul(1u64 << e.failures.min(20))
        .min(DIAL_BACKOFF_MAX_MS);
    e.next_ok_ms = now.saturating_add(delay);
}

/// Clear a peer's dial backoff (a successful connection, or a fresh heartbeat that
/// proves it's reachable).
fn reset_dial_backoff(backoff: &StdMutex<HashMap<Id, BackoffEntry>>, peer: &Id) {
    backoff.lock().unwrap().remove(peer);
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

    // Reject a used/stale invite up front — with a clear reason — so the joiner
    // gets a real message instead of silently failing the roster fold, and nobody
    // is prompted to approve a dead code.
    let invite_reason = {
        let st = inner.state.lock().await;
        match st.roster.check_invite(req.invite_kind, req.invite_nonce) {
            InviteCheck::Ok => None,
            InviteCheck::Spent => {
                Some("This invite code has already been used. Ask for a new one.".to_string())
            }
            InviteCheck::Stale => {
                Some("This invite code is no longer valid. Ask for a new one.".to_string())
            }
        }
    };
    if let Some(reason) = invite_reason {
        let _ = write_msg(&mut send, &JoinResponse::Denied(Some(reason))).await;
        let _ = send.finish();
        let _ = conn.closed().await;
        return;
    }

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
                JoinResponse::Denied(Some(format!("{e:#}")))
            }
        }
    } else {
        JoinResponse::Denied(None)
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
            // Confirm we actually landed in the roster before declaring success.
            // The admitting member's `Add` can still be rejected by the fold (e.g.
            // a single-use code consumed by a concurrent join, or a stale snapshot),
            // which would otherwise leave us "joined" but with no IP and no
            // membership. Poll the synced doc briefly; if we never appear, tear the
            // provisional network back down and report a clear error.
            let roster_cfg = cfg.roster_cfg();
            let mut joined = false;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let doc = inner.state.lock().await.doc.clone();
                if let Some(doc) = doc {
                    if let Ok(r) =
                        membership::build_roster(&roster_cfg, &doc, inner.node.blobs.blobs()).await
                    {
                        if r.is_member(&inner.my_id) {
                            joined = true;
                            break;
                        }
                    }
                }
            }
            if !joined {
                teardown(inner).await;
                bail!(
                    "the request was approved but this device wasn't added — the invite may have \
                     just been used or expired. Ask for a new code."
                );
            }
            let _ = inner.events.send(EngineEvent::Changed);
            Ok(())
        }
        JoinResponse::Denied(reason) => {
            bail!(reason.unwrap_or_else(|| "the member declined the join request".to_string()))
        }
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

/// Outcome of reconciling a freshly-handshaked connection against any existing one
/// to the same peer.
enum DupVerdict {
    /// No prior connection (or the same object) — adopt this one.
    Fresh,
    /// Adopt this one, evicting the superseded connection (closed off-lock).
    ReplaceOld(Connection),
    /// A connection we're keeping already exists — drop this duplicate.
    KeepExisting,
}

/// Human-readable direction for logs: which side opened the QUIC connection.
fn conn_dir(conn: &Connection) -> &'static str {
    match conn.side() {
        Side::Client => "outbound",
        Side::Server => "inbound",
    }
}

async fn register_mesh(inner: &Arc<Inner>, peer: Id, conn: Connection) {
    let dir = conn_dir(&conn);
    let new_id = conn.stable_id();

    // Reconcile against any existing connection to this peer, holding the sync
    // `conns` lock only for the decision (no await inside). Both ends run the same
    // tie-break, so a simultaneous double-dial converges on one shared connection
    // instead of each side keeping its own and later evicting the other's.
    let verdict = {
        let mut conns = inner.conns.write().unwrap();
        match conns.get(&peer) {
            Some(existing) if existing.stable_id() == new_id => DupVerdict::KeepExisting,
            Some(existing) => {
                let keep_new = resolve_duplicate(
                    &inner.my_id,
                    &peer,
                    existing.side(),
                    existing.close_reason().is_some(),
                    conn.side(),
                );
                if keep_new {
                    let old = conns.insert(peer, conn.clone()).expect("existing was present");
                    DupVerdict::ReplaceOld(old)
                } else {
                    DupVerdict::KeepExisting
                }
            }
            None => {
                conns.insert(peer, conn.clone());
                DupVerdict::Fresh
            }
        }
    };

    match verdict {
        DupVerdict::KeepExisting => {
            // Already holding a connection we're keeping; drop this duplicate without
            // touching presence (the peer is already online via the kept conn) or
            // spawning tasks for a conn we're closing. The remote runs the same rule.
            tracing::info!(
                "dropping duplicate {dir} mesh connection to {} (id {}); keeping existing",
                short(&peer),
                new_id
            );
            conn.close(0u32.into(), b"duplicate connection");
            return;
        }
        DupVerdict::ReplaceOld(old) => {
            tracing::warn!(
                "mesh connection to {} replaced by new {dir} connection (id {} supersedes id {})",
                short(&peer),
                new_id,
                old.stable_id()
            );
            // The superseded conn's watcher will fire, but its stable_id guard leaves
            // the new conn's map entry + presence intact.
            old.close(0u32.into(), b"superseded by duplicate");
        }
        DupVerdict::Fresh => {}
    }

    // This connection is now the live one for `peer`. Clear any dial backoff — this
    // also covers inbound connections, which never go through the outbound dialer.
    reset_dial_backoff(&inner.dial_backoff, &peer);
    {
        let mut st = inner.state.lock().await;
        st.presence.record_connection(peer, None, None, None, None, true);
    }
    tracing::info!(
        "mesh {dir} connection established to {} (id {})",
        short(&peer),
        new_id
    );
    let _ = inner.events.send(EngineEvent::Changed);

    // Inbound data plane: datagrams from this peer are IP packets — write them to
    // the TUN. Runs until the connection closes.
    {
        let inner = inner.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(pkt) = conn.read_datagram().await {
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
                        inner.blocked_inbound.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // Clamp inbound TCP SYNs too (bounds the other direction).
                    clamp_tcp_mss(&mut pkt, TUN_MSS);
                    let _ = tun.send(&pkt).await;
                }
            }
        });
    }

    // Watch for close: drop the connection + mark offline, but only if *this* conn is
    // still the map entry. A superseded duplicate closing must not evict the live one
    // (the bug that caused unexplained per-peer drops), so guard on `stable_id`.
    let inner2 = inner.clone();
    tokio::spawn(async move {
        let reason = conn.closed().await;
        let evicted = {
            let mut conns = inner2.conns.write().unwrap();
            match conns.get(&peer) {
                Some(cur) if cur.stable_id() == new_id => {
                    conns.remove(&peer);
                    true
                }
                _ => false,
            }
        };
        if evicted {
            // A deliberate close is routine; anything else is a real drop worth a warn
            // with the QUIC reason, so intermittent drops are diagnosable from the log.
            match &reason {
                ConnectionError::LocallyClosed | ConnectionError::ApplicationClosed(_) => {
                    tracing::info!(
                        "mesh connection to {} closed (id {}): {reason}",
                        short(&peer),
                        new_id
                    );
                }
                _ => tracing::warn!(
                    "mesh connection to {} lost (id {}): {reason}",
                    short(&peer),
                    new_id
                ),
            }
            let mut st = inner2.state.lock().await;
            st.presence.record_connection(peer, None, None, None, None, false);
            drop(st);
            let _ = inner2.events.send(EngineEvent::Changed);
        } else {
            tracing::debug!(
                "superseded mesh connection to {} closed (id {}): {reason}",
                short(&peer),
                new_id
            );
        }
    });
}

/// Tie-break for two mesh connections to the same peer (they arise when both ends
/// dial each other after a drop). Returns whether to keep the **new** connection.
///
/// The rule makes *both* endpoints converge on the same physical connection: the one
/// initiated by the lower NodeId wins. (If instead each side kept its own outbound,
/// they'd settle on different connections, each treating the other's kept conn as an
/// orphan and tearing it down on idle-timeout — a perpetual flap.) A re-dial from the
/// *same* initiator keeps the newer conn; an already-closed existing conn is always
/// replaced so a peer that restarted can reconnect immediately.
fn resolve_duplicate(
    my_id: &Id,
    peer: &Id,
    existing_side: Side,
    existing_closed: bool,
    new_side: Side,
) -> bool {
    if existing_closed {
        return true;
    }
    if existing_side == new_side {
        return true;
    }
    // Opposite directions = simultaneous open. Keep the connection whose initiator has
    // the lower NodeId; both ends compute the same winner from local information.
    let new_initiator_is_me = new_side == Side::Client;
    let i_am_lower = my_id < peer;
    new_initiator_is_me == i_am_lower
}

/// Bring up routing once we know our virtual IP.
///
/// On **desktop** this opens the OS TUN directly (best-effort: if it fails — no
/// elevation, missing wintun.dll, … — we log and keep running without routing, so
/// membership + presence still work) and spawns the outbound pump.
///
/// On **Android** we can't open a TUN ourselves — only `VpnService` can — so we
/// record the assigned IP and emit [`EngineEvent::TunSetupRequired`]; the app then
/// establishes the interface and calls [`Engine::attach_tun_fd`] with the fd.
async fn enable_tun(inner: &Arc<Inner>, ip: Ipv4Addr) {
    // Escape hatch for tests/CI (and headless runs where a TUN is undesirable).
    if std::env::var_os("NULLGATE_DISABLE_TUN").is_some() {
        return;
    }
    if inner.tun_attempted.swap(true, Ordering::SeqCst) {
        return; // only attempt once
    }
    *inner.assigned_ip.write().unwrap() = Some(ip);

    #[cfg(not(target_os = "android"))]
    match RealTun::open(ip, 24, TUN_MTU) {
        Ok(tun) => {
            let tun = Arc::new(tun);
            *inner.tun.write().unwrap() = Some(tun.clone());
            tracing::info!("routing enabled: TUN up at {ip}/24 (mtu {TUN_MTU})");
            spawn_tun_pump(inner, tun);
            let _ = inner.events.send(EngineEvent::Changed);
        }
        Err(e) => {
            tracing::warn!(
                "routing NOT enabled (TUN open failed: {e}). Membership/presence still work; \
                 run elevated (and ensure wintun.dll is present on Windows) for RDP/SSH routing."
            );
        }
    }

    #[cfg(target_os = "android")]
    {
        tracing::info!("routing pending: ask the app to bring up VpnService at {ip}/24");
        let _ = inner.events.send(EngineEvent::TunSetupRequired {
            ip: ip.to_string(),
            mtu: TUN_MTU as u32,
        });
    }
}

/// Spawn the outbound data plane: TUN packet → dst IP → peer → QUIC datagram.
/// Shared by the desktop open path and the Android fd-attach path; the abort
/// handle is tracked in `net_tasks` so leaving/disconnecting stops it cleanly.
fn spawn_tun_pump(inner: &Arc<Inner>, tun: Arc<RealTun>) {
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
            TransportAddr::Relay(url) if active => {
                relay = true;
                out.observed.get_or_insert_with(|| url.to_string());
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
#[cfg(not(target_os = "android"))]
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
#[cfg(not(target_os = "android"))]
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

/// Rebuild the in-memory roster from the (local-first) doc *now*, instead of waiting
/// for the next maintenance tick. Called right after we publish our own genesis +
/// invite in `create_network`: without it the creator's `st.roster` stays empty for
/// up to a tick interval, so a fast joiner (or an e2e test) is rejected with "invite
/// no longer valid" until the roster folds. Best-effort — on failure the tick catches
/// up. Mirrors the `was_member` latch the tick sets when we appear in the roster.
async fn refresh_roster(inner: &Arc<Inner>) {
    let (doc, cfg) = {
        let st = inner.state.lock().await;
        match (st.doc.clone(), st.config.clone()) {
            (Some(d), Some(c)) => (d, c),
            _ => return,
        }
    };
    if let Ok(roster) =
        membership::build_roster(&cfg.roster_cfg(), &doc, inner.node.blobs.blobs()).await
    {
        if roster.is_member(&inner.my_id) {
            inner.was_member.store(true, Ordering::SeqCst);
        }
        inner.state.lock().await.roster = roster;
    }
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

/// Which members to re-seed the roster-doc live-sync swarm with this tick, or
/// `None` to skip the (dialing) `start_sync` entirely.
///
/// - A membership **change** re-seeds *all* members (freshness: a new/removed/role-
///   changed entry must reach everyone).
/// - Otherwise, only the periodic self-heal re-seeds, and only **reachable** members
///   (a live mesh conn, or a fresh presence heartbeat per `is_fresh`). Re-dialing
///   *unreachable* members on a timer is the churn that grew iroh's mapped-address
///   cache (iroh#4293) and drove the watchdog restart loop.
///
/// Pure (no clock, no I/O) so the policy is unit-tested directly.
fn doc_reseed_targets(
    members: &std::collections::BTreeSet<Id>,
    connected: &std::collections::HashSet<Id>,
    is_fresh: impl Fn(&Id) -> bool,
    changed: bool,
    due: bool,
) -> Option<Vec<Id>> {
    if members.is_empty() {
        return None;
    }
    if changed {
        return Some(members.iter().copied().collect());
    }
    if !due {
        return None;
    }
    let targets: Vec<Id> = members
        .iter()
        .copied()
        .filter(|id| connected.contains(id) || is_fresh(id))
        .collect();
    (!targets.is_empty()).then_some(targets)
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

/// This device's shared display name. Normally the **actual current** OS hostname,
/// read fresh each call so it always reflects the real hostname (never cached,
/// never member-editable). If a process-wide override was set (Android, where the
/// OS hostname is meaningless), that wins — see [`crate::set_device_name_override`].
fn current_hostname() -> String {
    if let Some(name) = crate::device_name_override() {
        return name;
    }
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

/// Push a relay map into the **live** endpoint, off the request path.
///
/// `Endpoint::insert_relay`/`remove_relay` are not the cheap setters they look
/// like. Each one mutates the shared relay map synchronously and then *awaits*
/// iroh's socket-actor channel (bounded, 256) to nudge it into re-probing. That
/// actor can be blocked indefinitely: it awaits a per-remote `RemoteStateActor`
/// (inbox of 16), which awaits `poll_send` on the relay transport, which returns
/// `Pending` forever while the relay client for a path can't drain — e.g. a
/// token-gated relay that answers `401`. One peer stuck on a dead relay path
/// therefore backs up the whole chain and every `insert_relay` blocks with it.
/// That is the hang that made `relay add` sit for 20+ minutes.
///
/// So: a timeout per call, never one for the batch. The map mutation happens on
/// the first poll, before the blocking await — so a timed-out call has *still*
/// updated the map, and it's only the "re-probe now" nudge that was lost. But
/// the calls run in sequence, so without a per-call timeout the first stuck one
/// would stop every later insert from being polled at all, and the map would be
/// left half-written. The whole pass is idempotent, which is what lets us just
/// retry it.
///
/// If every attempt times out we say so ([`RelayApply::Failed`]) instead of
/// claiming success: the map is right, but nothing re-probed it, and the actor
/// that would have is wedged — so a restart is the honest advice.
///
/// [`RelayApply::Failed`]: crate::relays::RelayApply::Failed
async fn apply_relay_map(
    inner: Arc<Inner>,
    generation: u64,
    desired: Vec<Arc<iroh::RelayConfig>>,
    stale: Vec<iroh::RelayUrl>,
) {
    use crate::relays::RelayApply;

    /// Generous next to the sub-millisecond this takes on a healthy endpoint,
    /// but short enough that a wedged actor doesn't hold the pass for minutes.
    const CALL_TIMEOUT: Duration = Duration::from_secs(3);
    const ATTEMPTS: u32 = 3;
    const RETRY_BACKOFF: Duration = Duration::from_secs(5);

    // One applier at a time, so two rapid edits can't interleave.
    let _guard = inner.relay_apply_lock.lock().await;
    // A newer edit landed while we waited: its applier owns the endpoint now.
    if inner.relay_apply_gen.load(Ordering::SeqCst) != generation {
        return;
    }

    let ep = &inner.node.endpoint;
    let mut last_stuck = String::new();
    for attempt in 1..=ATTEMPTS {
        let mut stuck: Vec<String> = Vec::new();

        // Insert before removing, so the map is never momentarily empty.
        for cfg in &desired {
            if tokio::time::timeout(CALL_TIMEOUT, ep.insert_relay(cfg.url.clone(), cfg.clone()))
                .await
                .is_err()
            {
                stuck.push(cfg.url.to_string());
            }
        }
        for url in &stale {
            if tokio::time::timeout(CALL_TIMEOUT, ep.remove_relay(url))
                .await
                .is_err()
            {
                stuck.push(url.to_string());
            }
        }

        if stuck.is_empty() {
            tracing::info!(
                "relay map applied to the live endpoint: {} relay(s)",
                desired.len()
            );
            // Superseded mid-settle → the newer applier owns the state; say nothing.
            if let Some(apply) = settle_home_relay(&inner, generation, &desired).await {
                *inner.relay_apply.write().unwrap() = apply;
                let _ = inner.events.send(EngineEvent::Changed);
            }
            return;
        }

        last_stuck = stuck.join(", ");
        tracing::warn!(
            "iroh's socket actor did not acknowledge the relay map (attempt {attempt}/{ATTEMPTS}); \
             stuck on: {last_stuck}"
        );
        if inner.relay_apply_gen.load(Ordering::SeqCst) != generation {
            return;
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
    }

    let reason = format!(
        "iroh's socket actor is blocked and never acknowledged the new relay map \
         (stuck on: {last_stuck}); restart the daemon to apply it"
    );
    tracing::error!("{reason}");
    *inner.relay_apply.write().unwrap() = RelayApply::Failed { reason };
    let _ = inner.events.send(EngineEvent::Changed);
}

/// After the relay map is updated, wait for the endpoint's **home relay** — the
/// single relay it advertises, and the only one peers can reach us at — to land
/// inside the new map.
///
/// This exists because iroh keeps a home relay that has been removed from the
/// map. When a net-report finds nothing reachable it *re-injects* the current
/// home relay as the preferred one:
///
/// ```ignore
/// // iroh-1.0.0/src/socket.rs, handle_net_report_report()
/// if r.preferred_relay.is_none() && let Some(my_relay) = self.sock.my_relay() {
///     r.preferred_relay.replace(my_relay);
/// }
/// ```
///
/// so the home relay only ever *moves*, never clears. Removing it from the map
/// stops it being probed or dialed afresh, but an endpoint already homed on it
/// stays there until some other relay wins a report. Concretely: switch to
/// [`RelayPolicy::Only`] while your custom relay is unreachable, and the daemon
/// keeps using the public relay it was already on — exactly what `Only` promises
/// it won't. We can't force it off (iroh exposes no API for that), so we say so
/// and tell the user to restart, rather than reporting a success we didn't get.
///
/// Returns `None` if a newer settings change superseded us while we waited — it
/// owns the reported state now, and it is waiting on the lock we hold, so we must
/// not sit out the rest of the settle window.
///
/// [`RelayPolicy::Only`]: crate::relays::RelayPolicy::Only
async fn settle_home_relay(
    inner: &Arc<Inner>,
    generation: u64,
    desired: &[Arc<iroh::RelayConfig>],
) -> Option<crate::relays::RelayApply> {
    use crate::relays::RelayApply;
    use iroh::Watcher as _;

    /// The endpoint re-probes on a ~20-26s net-report cycle, so give it two.
    const SETTLE: Duration = Duration::from_secs(60);
    const POLL: Duration = Duration::from_millis(500);

    let desired_urls: std::collections::BTreeSet<&iroh::RelayUrl> =
        desired.iter().map(|c| &c.url).collect();
    let deadline = tokio::time::Instant::now() + SETTLE;

    loop {
        if inner.relay_apply_gen.load(Ordering::SeqCst) != generation {
            return None;
        }
        // The home relay, if we have one. No home relay is fine: it means no
        // relay is reachable, not that we're using one we shouldn't be.
        let home = inner
            .node
            .endpoint
            .home_relay_status()
            .get()
            .into_iter()
            .find(|s| s.is_connected())
            .map(|s| s.url().clone());
        let Some(home) = home else {
            return Some(RelayApply::Applied);
        };
        if desired_urls.contains(&home) {
            return Some(RelayApply::Applied);
        }
        if tokio::time::Instant::now() >= deadline {
            let reason = format!(
                "still using relay {home}, which is no longer configured — iroh keeps a \
                 home relay until another one takes over, and none of the configured \
                 relays answered. Restart the daemon to leave it."
            );
            tracing::warn!("{reason}");
            return Some(RelayApply::Failed { reason });
        }
        tokio::time::sleep(POLL).await;
    }
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

/// This client's local free-text notes about members (NodeId hex → note).
fn load_notes(data_dir: &Path) -> HashMap<String, String> {
    std::fs::read(data_dir.join("notes.cbor"))
        .ok()
        .and_then(|b| ciborium::from_reader(b.as_slice()).ok())
        .unwrap_or_default()
}

fn save_notes(data_dir: &Path, map: &HashMap<String, String>) {
    let _ = std::fs::create_dir_all(data_dir);
    let mut buf = Vec::new();
    if ciborium::into_writer(map, &mut buf).is_ok() {
        let _ = std::fs::write(data_dir.join("notes.cbor"), buf);
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

/// Regression tests for the daemon memory leak: the periodic tick fanned out a
/// brand-new `connect()` to every not-yet-connected member each interval, so an
/// unreachable member accrued iroh connection/path state without bound. The fix is
/// [`spawn_dials`] — dedup an in-flight dial + bound it with [`DIAL_TIMEOUT`]. These
/// drive it directly (no live node) on tokio's paused clock, asserting the exact
/// invariant the leak violated: **repeated ticks never stack dials, and the slot is
/// freed on timeout so retries still happen**.
#[cfg(test)]
mod dial_tests {
    use super::*;
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn id(n: u8) -> Id {
        [n; 32]
    }

    /// Yield enough times for any runnable spawned task to make progress under the
    /// current-thread test runtime (deterministic while the clock is paused).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_ticks_launch_at_most_one_dial_per_peer() {
        let dialing = DialingSet::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = id(1);

        // A dialer standing in for an unreachable member: it never resolves, so the
        // dial stays "in flight" exactly like a `connect()` that can't complete.
        let dial = {
            let calls = calls.clone();
            move |_peer: Id| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { pending::<Result<()>>().await }
            }
        };

        // The leak reproduced: 500 ticks for the same unreachable peer. Pre-fix this
        // spawned 500 dials; the guard must collapse them to a single in-flight one.
        for _ in 0..500 {
            spawn_dials(&dialing, vec![peer], dial.clone(), |_, _| {});
        }
        settle().await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "500 ticks must launch exactly one in-flight dial, not one per tick"
        );
        assert_eq!(dialing.lock().unwrap().len(), 1, "peer tracked as in-flight");
    }

    #[tokio::test(start_paused = true)]
    async fn dial_slot_frees_on_timeout_so_retry_can_happen() {
        let dialing = DialingSet::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = id(2);
        let dial = {
            let calls = calls.clone();
            move |_peer: Id| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { pending::<Result<()>>().await }
            }
        };

        spawn_dials(&dialing, vec![peer], dial.clone(), |_, _| {});
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(dialing.lock().unwrap().contains(&peer), "in flight before timeout");

        // Past the bound, the stuck dial is abandoned and its slot released — this is
        // what stops the unbounded accumulation while still permitting a later retry.
        tokio::time::advance(DIAL_TIMEOUT + Duration::from_secs(1)).await;
        settle().await;
        assert!(
            !dialing.lock().unwrap().contains(&peer),
            "slot must be freed once the dial times out"
        );

        // The next tick is now free to re-dial the (still unreachable) peer.
        spawn_dials(&dialing, vec![peer], dial.clone(), |_, _| {});
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "retry after the slot freed");
    }

    #[tokio::test(start_paused = true)]
    async fn distinct_peers_each_get_their_own_dial() {
        // Dedup is per-peer, not global: independent members must all be dialed.
        let dialing = DialingSet::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let dial = {
            let calls = calls.clone();
            move |_peer: Id| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { pending::<Result<()>>().await }
            }
        };

        let peers = vec![id(1), id(2), id(3)];
        spawn_dials(&dialing, peers.clone(), dial.clone(), |_, _| {});
        // A second tick with the same set must add nothing (all already in flight).
        spawn_dials(&dialing, peers, dial.clone(), |_, _| {});
        settle().await;

        assert_eq!(calls.load(Ordering::SeqCst), 3, "one dial per distinct peer");
        assert_eq!(dialing.lock().unwrap().len(), 3);
    }
}

/// Tests for the duplicate-connection tie-break that fixes the unexplained per-peer
/// drops: a simultaneous double-dial must converge on **one** connection at *both*
/// ends, so neither side later evicts the connection the other kept.
#[cfg(test)]
mod dup_tests {
    use super::*;

    fn id(n: u8) -> Id {
        [n; 32]
    }

    #[test]
    fn already_closed_existing_is_replaced() {
        // A peer that restarted re-dials; our stale entry is closed → adopt the new
        // conn immediately regardless of direction.
        for existing in [Side::Client, Side::Server] {
            for new in [Side::Client, Side::Server] {
                assert!(
                    resolve_duplicate(&id(1), &id(2), existing, true, new),
                    "closed existing must always be replaced"
                );
            }
        }
    }

    #[test]
    fn same_direction_duplicate_prefers_newer() {
        // Re-dial from the same initiator (both inbound, or both outbound): the newer
        // connection wins.
        assert!(resolve_duplicate(&id(1), &id(2), Side::Server, false, Side::Server));
        assert!(resolve_duplicate(&id(1), &id(2), Side::Client, false, Side::Client));
    }

    #[test]
    fn simultaneous_open_converges_on_lower_initiator_at_both_ends() {
        // Peers A(=1) and B(=2), A < B. Two connections exist for the pair:
        //   c1: A dialed B  → A sees side=Client, B sees side=Server
        //   c2: B dialed A  → B sees side=Client, A sees side=Server
        // Both ends must keep c1 (initiated by the lower id, A), whatever order the
        // two registrations arrive in on each side.
        let (a, b) = (id(1), id(2));

        // --- A's side ---
        // c1 registered first, then c2 arrives: keep existing c1 (reject new c2).
        assert!(
            !resolve_duplicate(&a, &b, Side::Client, false, Side::Server),
            "A: c2 (inbound) must not displace c1"
        );
        // c2 registered first, then c1 arrives: replace with new c1.
        assert!(
            resolve_duplicate(&a, &b, Side::Server, false, Side::Client),
            "A: c1 (outbound) must displace c2"
        );

        // --- B's side ---
        // c1 registered first (inbound to B), then c2 arrives: keep c1.
        assert!(
            !resolve_duplicate(&b, &a, Side::Server, false, Side::Client),
            "B: c2 (outbound) must not displace c1"
        );
        // c2 registered first, then c1 arrives (inbound): replace with c1.
        assert!(
            resolve_duplicate(&b, &a, Side::Client, false, Side::Server),
            "B: c1 (inbound) must displace c2"
        );
    }

    /// The core convergence property: for any registration order on each side, A and B
    /// end up holding the connection with the *same* initiator.
    #[test]
    fn both_ends_pick_the_same_initiator() {
        for (lo, hi) in [(id(1), id(2)), (id(7), id(200))] {
            // The winning connection is the one initiated by `lo`. Verify each side,
            // starting from either connection as the incumbent, keeps that one.
            // A(lo) keeps the conn it initiated (Client); rejects the peer-initiated one.
            assert!(resolve_duplicate(&lo, &hi, Side::Server, false, Side::Client));
            assert!(!resolve_duplicate(&lo, &hi, Side::Client, false, Side::Server));
            // B(hi) keeps the conn lo initiated (which is inbound/Server to B); rejects
            // the one B itself initiated.
            assert!(resolve_duplicate(&hi, &lo, Side::Client, false, Side::Server));
            assert!(!resolve_duplicate(&hi, &lo, Side::Server, false, Side::Client));
        }
    }
}

/// Tests for the roster-doc re-seed target selection: a membership change re-seeds
/// everyone, but the periodic self-heal only re-seeds *reachable* members so we don't
/// re-dial unreachable ones on a timer (the churn behind the watchdog restart loop).
#[cfg(test)]
mod reseed_tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    fn id(n: u8) -> Id {
        [n; 32]
    }

    #[test]
    fn empty_membership_never_seeds() {
        let members = BTreeSet::new();
        let connected = HashSet::new();
        assert!(doc_reseed_targets(&members, &connected, |_| false, true, true).is_none());
    }

    #[test]
    fn membership_change_seeds_all_members_even_if_unreachable() {
        let members: BTreeSet<Id> = [id(1), id(2), id(3)].into_iter().collect();
        let connected = HashSet::new(); // none connected
        let got = doc_reseed_targets(&members, &connected, |_| false, true, false)
            .expect("change must seed");
        assert_eq!(got.len(), 3, "a membership change re-seeds every member");
    }

    #[test]
    fn not_due_and_unchanged_skips() {
        let members: BTreeSet<Id> = [id(1)].into_iter().collect();
        let connected: HashSet<Id> = [id(1)].into_iter().collect();
        assert!(
            doc_reseed_targets(&members, &connected, |_| true, false, false).is_none(),
            "no change and not due → nothing to do"
        );
    }

    #[test]
    fn periodic_selfheal_targets_only_reachable_members() {
        let members: BTreeSet<Id> = [id(1), id(2), id(3), id(4)].into_iter().collect();
        let connected: HashSet<Id> = [id(1)].into_iter().collect(); // 1 has a live conn
        let fresh: HashSet<Id> = [id(2)].into_iter().collect(); // 2 has a fresh heartbeat
        // 3 and 4 are unreachable and must be skipped.
        let mut got =
            doc_reseed_targets(&members, &connected, |x| fresh.contains(x), false, true)
                .expect("reachable members present");
        got.sort();
        assert_eq!(got, vec![id(1), id(2)], "only conn-held or fresh members");
    }

    #[test]
    fn periodic_selfheal_with_no_reachable_members_skips() {
        let members: BTreeSet<Id> = [id(3), id(4)].into_iter().collect();
        let connected = HashSet::new();
        assert!(
            doc_reseed_targets(&members, &connected, |_| false, false, true).is_none(),
            "nothing reachable → skip the dialing start_sync entirely"
        );
    }
}

/// Tests for the per-peer dial backoff: a persistently unreachable member is retried
/// ever more sparsely (never a flat ~20s forever, never dropped), and any sign of
/// life — a successful connection or a fresh heartbeat — resets it to immediate.
#[cfg(test)]
mod backoff_tests {
    use super::*;
    use std::collections::HashMap;

    fn id(n: u8) -> Id {
        [n; 32]
    }

    #[test]
    fn no_entry_always_passes_the_filter() {
        let backoff = HashMap::new();
        let got = dial_backoff_filter(vec![id(1), id(2)], &backoff, 1_000);
        assert_eq!(got.len(), 2, "peers that never failed are always dialable");
    }

    #[test]
    fn backed_off_peer_is_held_until_its_window_elapses() {
        let backoff = StdMutex::new(HashMap::new());
        record_dial_failure(&backoff, id(1), 0);
        // First failure delays by DIAL_TIMEOUT*2 = 40s; still held at 30s, freed at 40s.
        let map = backoff.lock().unwrap().clone();
        assert!(
            dial_backoff_filter(vec![id(1)], &map, 30_000).is_empty(),
            "still inside the backoff window"
        );
        assert_eq!(
            dial_backoff_filter(vec![id(1)], &map, 40_000),
            vec![id(1)],
            "dialable once the window elapses"
        );
    }

    #[test]
    fn window_grows_with_consecutive_failures_and_caps() {
        let backoff = StdMutex::new(HashMap::new());
        let mut prev = 0u64;
        for _ in 0..12 {
            record_dial_failure(&backoff, id(1), 0);
            let next = backoff.lock().unwrap().get(&id(1)).unwrap().next_ok_ms;
            assert!(next >= prev, "backoff is monotonic across failures");
            assert!(next <= DIAL_BACKOFF_MAX_MS, "never exceeds the cap");
            prev = next;
        }
        assert_eq!(prev, DIAL_BACKOFF_MAX_MS, "saturates at the cap");
    }

    #[test]
    fn reset_clears_the_backoff() {
        let backoff = StdMutex::new(HashMap::new());
        record_dial_failure(&backoff, id(1), 0);
        reset_dial_backoff(&backoff, &id(1));
        let map = backoff.lock().unwrap().clone();
        assert_eq!(
            dial_backoff_filter(vec![id(1)], &map, 0),
            vec![id(1)],
            "a reset peer is immediately dialable again"
        );
    }
}
