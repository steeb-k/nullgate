//! Nullgate mobile facade.
//!
//! This crate is the Android equivalent of `ipn-daemon`: it owns a tokio runtime
//! and a single [`ipn_core::Engine`], and exposes a flat, UniFFI-friendly API the
//! Kotlin/Compose app drives directly — no socket, no separate process. Where the
//! desktop GUI subscribes to the daemon's [`ipn_ipc::IpcEvent`] stream over a
//! socket, the app implements an [`EventListener`] callback that an event-bridge
//! task feeds from `engine.subscribe()`.
//!
//! Threading model: the exported methods are *synchronous* and drive the engine by
//! `block_on`-ing the owned runtime. The Android side calls them off the main
//! thread (a background `Dispatchers.IO` coroutine inside the foreground service),
//! so blocking is fine and the lifecycle stays explicit. The engine's own
//! maintenance loop and the event bridge run as tasks *on* that runtime.
//!
//! Because the engine emits [`EngineEvent::Changed`] itself on every state change,
//! the facade does not manually notify after each mutation — it relies on the event
//! stream, exactly like the desktop GUI's `Subscribe` path.

use std::net::Ipv4Addr;
use std::sync::Arc;

use ipn_core::{Engine, EngineEvent, Pace};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::broadcast;

uniffi::setup_scaffolding!();

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Every fallible facade method surfaces engine/anyhow failures as this flat
/// error; UniFFI maps it to a thrown exception carrying the message on the Kotlin
/// side.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum NullgateError {
    #[error("{0}")]
    Engine(String),
}

impl From<anyhow::Error> for NullgateError {
    fn from(e: anyhow::Error) -> Self {
        // `{:#}` includes the full anyhow context chain, matching the daemon's
        // diagnostics.
        NullgateError::Engine(format!("{e:#}"))
    }
}

type Result<T> = std::result::Result<T, NullgateError>;

// ---------------------------------------------------------------------------
// Records — mirror ipn-core's display DTOs 1:1 (UniFFI can't reuse the external
// serde types, so we re-declare them and convert with `From`).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, uniffi::Record)]
pub struct MemberView {
    pub node_id: String,
    pub hostname: Option<String>,
    pub label: Option<String>,
    pub note: Option<String>,
    pub virtual_ip: Option<String>,
    pub local_ip: Option<String>,
    pub public_ip: Option<String>,
    pub location: Option<String>,
    pub observed_addr: Option<String>,
    pub direct: Option<bool>,
    pub online: bool,
    pub last_seen: u64,
    pub is_self: bool,
    pub is_originator_device: bool,
    pub role: String,
    pub access_disabled: bool,
    pub hidden: bool,
}

impl From<ipn_core::MemberView> for MemberView {
    fn from(m: ipn_core::MemberView) -> Self {
        MemberView {
            node_id: m.node_id,
            hostname: m.hostname,
            label: m.label,
            note: m.note,
            virtual_ip: m.virtual_ip,
            local_ip: m.local_ip,
            public_ip: m.public_ip,
            location: m.location,
            observed_addr: m.observed_addr,
            direct: m.direct,
            online: m.online,
            last_seen: m.last_seen,
            is_self: m.is_self,
            is_originator_device: m.is_originator_device,
            role: m.role,
            access_disabled: m.access_disabled,
            hidden: m.hidden,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NetworkStatus {
    pub name: String,
    pub subnet: String,
    pub frozen: bool,
    pub self_node_id: String,
    pub self_ip: Option<String>,
    pub is_originator: bool,
    pub self_role: String,
    pub peer_ticket_single_use: bool,
    pub routing: bool,
    pub online: bool,
    pub home_relay: Option<String>,
    pub members: Vec<MemberView>,
}

impl From<ipn_core::NetworkStatus> for NetworkStatus {
    fn from(s: ipn_core::NetworkStatus) -> Self {
        NetworkStatus {
            name: s.name,
            subnet: s.subnet,
            frozen: s.frozen,
            self_node_id: s.self_node_id,
            self_ip: s.self_ip,
            is_originator: s.is_originator,
            self_role: s.self_role,
            peer_ticket_single_use: s.peer_ticket_single_use,
            routing: s.routing,
            online: s.online,
            home_relay: s.home_relay,
            members: s.members.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AuditEntry {
    pub ts: u64,
    pub actor_node_id: String,
    pub actor_name: Option<String>,
    pub action: String,
}

// --- Custom relay servers ---------------------------------------------------
//
// Same per-device model as the desktop: `relays.cbor` in the app data dir, not
// distributed through the roster. The phone was the only device that *couldn't*
// be put on the self-hosted relay (there was no mobile surface at all), which is
// the only reason it stayed reachable through the July 2026 partition.

/// One user-configured relay server.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RelayServer {
    pub url: String,
    pub token: Option<String>,
}

/// How custom relays combine with the public iroh relays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RelayPolicy {
    /// Custom relays carry the traffic, but the public relays stay in the map so
    /// peers that don't have your relay can still reach you.
    Preferred,
    /// Custom relays exclusively. Peers without the relay cannot reach you.
    Only,
}

/// How far the last relay-settings change got in reaching the live endpoint.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum RelayApply {
    Applied,
    Pending,
    Failed { reason: String },
}

/// The device's relay configuration plus its apply state.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RelayStatus {
    pub servers: Vec<RelayServer>,
    pub mode: RelayPolicy,
    pub apply: RelayApply,
}

impl From<ipn_core::RelayServer> for RelayServer {
    fn from(s: ipn_core::RelayServer) -> Self {
        RelayServer { url: s.url, token: s.token }
    }
}

impl From<RelayServer> for ipn_core::RelayServer {
    fn from(s: RelayServer) -> Self {
        ipn_core::RelayServer { url: s.url, token: s.token }
    }
}

impl From<ipn_core::RelayPolicy> for RelayPolicy {
    fn from(m: ipn_core::RelayPolicy) -> Self {
        match m {
            ipn_core::RelayPolicy::Preferred => RelayPolicy::Preferred,
            ipn_core::RelayPolicy::Only => RelayPolicy::Only,
        }
    }
}

impl From<RelayPolicy> for ipn_core::RelayPolicy {
    fn from(m: RelayPolicy) -> Self {
        match m {
            RelayPolicy::Preferred => ipn_core::RelayPolicy::Preferred,
            RelayPolicy::Only => ipn_core::RelayPolicy::Only,
        }
    }
}

impl From<ipn_core::RelayApply> for RelayApply {
    fn from(a: ipn_core::RelayApply) -> Self {
        match a {
            ipn_core::RelayApply::Applied => RelayApply::Applied,
            ipn_core::RelayApply::Pending => RelayApply::Pending,
            ipn_core::RelayApply::Failed { reason } => RelayApply::Failed { reason },
        }
    }
}

impl From<ipn_core::RelayStatus> for RelayStatus {
    fn from(s: ipn_core::RelayStatus) -> Self {
        RelayStatus {
            servers: s.settings.servers.into_iter().map(Into::into).collect(),
            mode: s.settings.mode.into(),
            apply: s.apply.into(),
        }
    }
}

impl From<ipn_core::AuditEntry> for AuditEntry {
    fn from(e: ipn_core::AuditEntry) -> Self {
        AuditEntry {
            ts: e.ts,
            actor_node_id: e.actor_node_id,
            actor_name: e.actor_name,
            action: e.action,
        }
    }
}

// ---------------------------------------------------------------------------
// Event callback — the Android-side observer, replacing the socket subscription.
// ---------------------------------------------------------------------------

/// Implemented in Kotlin and registered once at init. The event-bridge task
/// invokes it to push live state changes (mirroring the daemon's broadcast of
/// [`ipn_ipc::IpcEvent`]). The app refreshes its UI from these signals.
#[uniffi::export(callback_interface)]
pub trait EventListener: Send + Sync + 'static {
    /// Something about the network/members changed — re-query [`MobileEngine::status`].
    fn on_changed(&self);
    /// We (the joiner) computed the SAS for a join in progress — show the emojis so
    /// the user can compare them with the approving member's screen.
    fn on_join_sas(&self, sas: Vec<String>);
    /// A device wants to join; an existing member should compare `sas` and approve.
    fn on_join_request(&self, node_id: String, hostname: String, sas: Vec<String>);
    /// Routing needs the platform TUN: bring up the `VpnService` at `ip`/24 with
    /// `mtu`, then hand the fd back via [`MobileEngine::attach_tun`].
    fn on_tun_setup_required(&self, ip: String, mtu: u32);
    /// Routing is going away — tear down the `VpnService`.
    fn on_tun_teardown_required(&self);
}

// ---------------------------------------------------------------------------
// The engine object
// ---------------------------------------------------------------------------

struct Inner {
    /// `ipn_core::Engine` is `Clone` and internally synchronized, so it needs no
    /// outer lock here (unlike seed-core).
    engine: Engine,
    listener: Box<dyn EventListener>,
}

/// The single long-lived object the Android foreground service holds. Construct
/// once with [`MobileEngine::init`]; it boots the engine and starts the event
/// bridge immediately. Drop it (or call [`MobileEngine::shutdown`]) to stop.
#[derive(uniffi::Object)]
pub struct MobileEngine {
    rt: Runtime,
    inner: Arc<Inner>,
}

#[uniffi::export]
impl MobileEngine {
    /// Boot the engine against `data_dir` (app-private internal storage, holding
    /// `node.key`, `network.cbor`, `docs/`, `blobs/`, `secrets/`). `device_name`
    /// is the stable, user-uneditable name other members see (the OS hostname is
    /// meaningless on Android); set it once here before the engine starts.
    #[uniffi::constructor]
    pub fn init(
        data_dir: String,
        device_name: String,
        listener: Box<dyn EventListener>,
    ) -> Result<Arc<Self>> {
        init_android_logging();
        ipn_core::set_device_name_override(device_name);

        // A small fixed pool rather than one worker per core: the engine is I/O-bound
        // (a phone's mesh is a handful of connections), and two workers keep a blocking
        // `block_on` dispatch from starving the data-plane pump without spinning up a
        // thread per core on an 8-core phone.
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| NullgateError::Engine(format!("create runtime: {e}")))?;
        let engine = rt.block_on(async { Engine::start(&data_dir).await })?;

        let inner = Arc::new(Inner { engine, listener });
        let rx = inner.engine.subscribe();
        rt.spawn(event_bridge(inner.clone(), rx));

        Ok(Arc::new(MobileEngine { rt, inner }))
    }

    // --- Status / read-only ------------------------------------------------

    /// Full network + member snapshot for the UI. Errors if no network is active.
    pub fn status(&self) -> Result<NetworkStatus> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.status().await })?
            .into())
    }

    /// Whether a network is currently configured (joined/created), regardless of
    /// online state.
    pub fn has_network(&self) -> bool {
        self.rt.block_on(async { self.inner.engine.has_network().await })
    }

    /// This device's NodeId (hex) — the cryptographic identity other members trust.
    pub fn self_node_id(&self) -> String {
        self.inner.engine.self_node_id_hex()
    }

    /// Number of live mesh connections (diagnostics).
    pub fn live_connection_count(&self) -> u32 {
        self.inner.engine.live_connection_count() as u32
    }

    /// The 30-day administration activity log (derived from the signed roster).
    pub fn audit_log(&self) -> Result<Vec<AuditEntry>> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.audit_log().await })?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    // --- Network lifecycle -------------------------------------------------

    /// Create a new network named `name` on the standard `10.99.0.0/24` subnet
    /// (matching the desktop daemon). Returns the join ticket.
    pub fn create_network(&self, name: String) -> Result<String> {
        Ok(self.rt.block_on(async {
            self.inner
                .engine
                .create_network(name, Ipv4Addr::new(10, 99, 0, 0))
                .await
        })?)
    }

    /// Join an existing network from a `ng1…` ticket string.
    pub fn join_network(&self, ticket: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.join_network(&ticket).await })?)
    }

    /// Leave the network entirely (forget the config + secrets; keep the device key).
    pub fn leave_network(&self) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.leave_network().await })?)
    }

    /// Go online (resume the saved network) or offline (disconnect, keep config).
    pub fn set_online(&self, online: bool) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_online(online).await })?)
    }

    /// Originator: dissolve the network for everyone.
    pub fn delete_network(&self) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.delete_network().await })?)
    }

    /// Originator: mass-revoke and rotate to a fresh secret; returns the new ticket.
    pub fn rotate_network(&self) -> Result<String> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.rotate_network().await })?)
    }

    /// Rename the network (controllers+).
    pub fn set_network_name(&self, name: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_network_name(name).await })?)
    }

    /// Freeze/unfreeze the network (no new joins while frozen).
    pub fn set_frozen(&self, frozen: bool) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_frozen(frozen).await })?)
    }

    // --- Admission / membership -------------------------------------------

    pub fn approve_join(&self, node_id: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.approve_join(&node_id).await })?)
    }

    pub fn deny_join(&self, node_id: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.deny_join(&node_id).await })?)
    }

    pub fn remove_member(&self, node_id: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.remove_member(&node_id).await })?)
    }

    /// Promote/demote a member between Peer and Controller (originator only).
    pub fn set_member_role(&self, node_id: String, controller: bool) -> Result<()> {
        Ok(self.rt.block_on(async {
            self.inner
                .engine
                .set_member_role(&node_id, controller)
                .await
        })?)
    }

    // --- Tickets -----------------------------------------------------------

    /// The current Peer-level join ticket.
    pub fn ticket(&self) -> Result<String> {
        Ok(self.rt.block_on(async { self.inner.engine.ticket().await })?)
    }

    /// A single-use Controller-level join ticket (originator only).
    pub fn controller_ticket(&self) -> Result<String> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.controller_ticket().await })?)
    }

    /// Toggle whether Peer join tickets are single-use.
    pub fn set_peer_ticket_single_use(&self, on: bool) -> Result<()> {
        Ok(self.rt.block_on(async {
            self.inner.engine.set_peer_ticket_single_use(on).await
        })?)
    }

    // --- Originator key ----------------------------------------------------

    /// Export the originator master key as a `ngkey1…` recovery string.
    pub fn export_originator_key(&self) -> Result<String> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.export_originator_key().await })?)
    }

    /// Import an originator master key from a `ngkey1…` recovery string.
    pub fn import_originator_key(&self, code: String) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.import_originator_key(&code).await })?)
    }

    // --- Per-device toggles + local annotations ---------------------------

    /// Block inbound remote access to this device (one-way; outbound still works).
    pub fn set_remote_access_disabled(&self, disabled: bool) -> Result<()> {
        Ok(self.rt.block_on(async {
            self.inner
                .engine
                .set_remote_access_disabled(disabled)
                .await
        })?)
    }

    /// Ask to be hidden from the member list (implies the inbound block).
    pub fn set_hidden(&self, hidden: bool) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_hidden(hidden).await })?)
    }

    /// Set/clear a local-only friendly nickname for a member (never broadcast).
    pub fn set_nickname(&self, node_id: String, name: Option<String>) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_nickname(&node_id, name).await })?)
    }

    /// Set/clear a local-only free-text note about a member (never broadcast).
    pub fn set_note(&self, node_id: String, note: Option<String>) -> Result<()> {
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_note(&node_id, note).await })?)
    }

    // --- Custom relay servers ---------------------------------------------

    /// This device's relay configuration and how far it got in reaching the live
    /// endpoint.
    pub fn relay_status(&self) -> RelayStatus {
        self.inner.engine.relay_status().into()
    }

    /// Replace this device's relay configuration. Returns once it is saved; it
    /// reaches the running endpoint in the background, so re-read
    /// [`relay_status`](Self::relay_status) for the verdict rather than assuming
    /// this call means "applied".
    ///
    /// Per-device, like the desktop: the same servers and tokens have to be set
    /// on every member, or the ones without them can't be reached.
    pub fn set_relay_settings(&self, servers: Vec<RelayServer>, mode: RelayPolicy) -> Result<()> {
        let settings = ipn_core::RelaySettings {
            servers: servers.into_iter().map(Into::into).collect(),
            mode: mode.into(),
        };
        Ok(self
            .rt
            .block_on(async { self.inner.engine.set_relay_settings(settings).await })?)
    }

    // --- VpnService coordination ------------------------------------------

    /// Our roster-assigned virtual IP (`10.99.0.x`), once known — read after
    /// [`EventListener::on_tun_setup_required`] to build the `VpnService`.
    pub fn assigned_ip(&self) -> Option<String> {
        self.inner.engine.assigned_ip()
    }

    /// Adopt the TUN fd from the app's `VpnService` (`ParcelFileDescriptor
    /// .detachFd()`) and start routing. Android only; takes ownership of `fd`.
    pub fn attach_tun(&self, fd: i32) -> Result<()> {
        #[cfg(target_os = "android")]
        {
            // Must run inside the runtime: adopting the fd registers it with the
            // tokio reactor (`AsyncFd`) and the pump is `tokio::spawn`ed — both
            // panic ("no reactor running") if called outside a runtime context.
            self.rt
                .block_on(async { self.inner.engine.attach_tun_fd(fd) })?;
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = fd;
            Err(NullgateError::Engine(
                "attach_tun is only available on Android".into(),
            ))
        }
    }

    /// Drop the TUN (VPN revoked/stopping) and stop routing. Android only; a no-op
    /// elsewhere.
    pub fn detach_tun(&self) {
        #[cfg(target_os = "android")]
        // In the runtime context too: dropping the `AsyncFd`-backed device
        // deregisters it from the reactor.
        self.rt.block_on(async { self.inner.engine.detach_tun() });
    }

    // --- Lifecycle ---------------------------------------------------------

    /// Switch the engine's maintenance cadence to match app visibility. Call with
    /// `background = true` when the app is backgrounded / the screen is off (slows
    /// the housekeeping loop and presence heartbeat to save battery) and `false`
    /// when it returns to the foreground. Cheap and synchronous.
    pub fn set_pace(&self, background: bool) {
        let pace = if background {
            Pace::Background
        } else {
            Pace::Interactive
        };
        self.inner.engine.set_pace(pace);
    }

    /// Tell the engine the device's connectivity changed (from Android's
    /// `ConnectivityManager` — the only such signal on Android). Rebinds iroh's
    /// sockets and kicks a recovery burst so peers become visible again after
    /// another VPN released the network, without waiting for anything to time out.
    /// Safe to call spuriously.
    pub fn network_changed(&self) {
        self.rt
            .block_on(async { self.inner.engine.network_changed().await });
    }

    /// Best-effort graceful stop: go offline. The node is fully torn down when the
    /// object is dropped (the foreground service drops its reference on stop).
    pub fn shutdown(&self) {
        let _ = self
            .rt
            .block_on(async { self.inner.engine.set_online(false).await });
    }
}

/// Forward engine events to the Kotlin [`EventListener`]. Runs on the runtime for
/// the engine's lifetime; a dropped/closed channel ends it.
async fn event_bridge(inner: Arc<Inner>, mut rx: broadcast::Receiver<EngineEvent>) {
    loop {
        match rx.recv().await {
            Ok(EngineEvent::Changed) => inner.listener.on_changed(),
            Ok(EngineEvent::JoinSas { sas }) => inner.listener.on_join_sas(sas),
            Ok(EngineEvent::JoinRequest {
                node_id,
                hostname,
                sas,
            }) => inner.listener.on_join_request(node_id, hostname, sas),
            Ok(EngineEvent::TunSetupRequired { ip, mtu }) => {
                inner.listener.on_tun_setup_required(ip, mtu)
            }
            Ok(EngineEvent::TunTeardownRequired) => inner.listener.on_tun_teardown_required(),
            // We fell behind the broadcast buffer: collapse the gap into a single
            // refresh, since the UI re-queries full state on `on_changed`.
            Err(broadcast::error::RecvError::Lagged(_)) => inner.listener.on_changed(),
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// On Android, route `tracing` to Logcat once. No-op elsewhere (and idempotent — a
/// second init is ignored).
#[cfg(target_os = "android")]
fn init_android_logging() {
    use std::sync::Once;
    static START: Once = Once::new();
    START.call_once(|| {
        use tracing_subscriber::prelude::*;
        let _ = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "ipn_mobile=info,ipn_core=info".into()),
            )
            .with(paranoid_android::layer("nullgate"))
            .try_init();
    });
}

#[cfg(not(target_os = "android"))]
fn init_android_logging() {}

/// JNI entry point: hand the app's JVM `Context` to the two Android-aware pieces of
/// the iroh stack so they can reach platform services:
///
/// * **`ndk-context`** — iroh's DNS discovery (hickory-resolver) reads Android's
///   DNS config through it. Unset, the first resolver call panics "android context
///   was not initialized".
/// * **`rustls-platform-verifier`** — iroh's relay TLS validates certificates
///   against Android's trust store through it.
///
/// The Kotlin `RustlsSetup` object declares this as an `external fun` and calls it
/// once in `Application.onCreate`, before the engine starts.
///
/// Symbol name maps to `io.github.steeb_k.nullgate.RustlsSetup` (the `_1` escapes
/// the underscore in `steeb_k`, per JNI name mangling).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_steeb_1k_nullgate_RustlsSetup_initRustlsPlatformVerifier<
    'caller,
>(
    mut env: jni::EnvUnowned<'caller>,
    _this: jni::objects::JObject<'caller>,
    context: jni::objects::JObject<'caller>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        // ndk-context wants the raw JavaVM + a long-lived global ref to the Context
        // (it stores the pointers for the process lifetime). `into_raw` leaks the
        // global ref intentionally so the jobject stays valid.
        let vm = env.get_java_vm()?;
        let ctx_global = env.new_global_ref(&context)?;
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_raw() as *mut std::ffi::c_void,
                ctx_global.into_raw() as *mut std::ffi::c_void,
            );
        }
        rustls_platform_verifier::android::init_with_env(env, context)?;
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
