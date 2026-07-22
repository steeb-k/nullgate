package io.github.steeb_k.nullgate.engine

import android.content.Context
import android.net.VpnService
import android.os.Build
import android.provider.Settings
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.ipn_mobile.AuditEntry
import uniffi.ipn_mobile.EventListener
import uniffi.ipn_mobile.MemberView
import uniffi.ipn_mobile.MobileEngine
import uniffi.ipn_mobile.NetworkStatus
import uniffi.ipn_mobile.RelayPolicy
import uniffi.ipn_mobile.RelayServer
import uniffi.ipn_mobile.RelayStatus
import java.io.File

/**
 * Process-wide owner of the one [MobileEngine]. The foreground
 * [NullgateVpnService] boots it; the Compose UI observes the [StateFlow]s here and
 * issues mutations through the suspend wrappers. Mirrors the desktop split where
 * the GUI is a thin client of the daemon — except here engine and UI share a
 * process, so the "IPC" is direct method calls plus the [EventListener] callback.
 *
 * The engine drives routing on Android via [EventListener.onTunSetupRequired] /
 * [EventListener.onTunTeardownRequired]; this holder relays those to the VPN
 * service, gating actual `VpnService` establishment on the user's consent.
 */
object EngineHolder {
    private const val TAG = "EngineHolder"

    /** Matches `ipn_core::router::TUN_MTU`; used when establishing the VPN after a
     * deferred consent (the engine's setup event carries it for the immediate path). */
    const val TUN_MTU: Int = 1280

    private val _status = MutableStateFlow<NetworkStatus?>(null)
    val status: StateFlow<NetworkStatus?> = _status.asStateFlow()

    private val _running = MutableStateFlow(false)
    val running: StateFlow<Boolean> = _running.asStateFlow()

    /** SAS emojis to display while *we* are joining a network (compare on approver). */
    private val _joinSas = MutableStateFlow<List<String>?>(null)
    val joinSas: StateFlow<List<String>?> = _joinSas.asStateFlow()

    /** Incoming join requests awaiting our approval (keyed by node id). */
    private val _pendingJoins = MutableStateFlow<List<PendingJoin>>(emptyList())
    val pendingJoins: StateFlow<List<PendingJoin>> = _pendingJoins.asStateFlow()

    /** True when routing wants to come up but the user hasn't granted VPN consent. */
    private val _needsVpnConsent = MutableStateFlow(false)
    val needsVpnConsent: StateFlow<Boolean> = _needsVpnConsent.asStateFlow()

    /** Whether the app is in the foreground (screen on AND activity resumed). Drives
     * the engine's maintenance pace and the mDNS multicast lock — both are pure
     * battery cost while nobody's looking. The service observes this. */
    private val _foreground = MutableStateFlow(false)
    val foreground: StateFlow<Boolean> = _foreground.asStateFlow()

    @Volatile
    private var activityResumed = false

    @Volatile
    private var screenInteractive = true

    /** The user's *intended* online state, so an automatic resume after a foreign VPN
     * releases the network never overrides a deliberate "go offline". Defaults true
     * (the common case); flipped false only on an explicit offline / leave. */
    @Volatile
    private var userWantsOnline = true

    /** Set when another VPN revoked our tunnel; cleared once we auto-resume. */
    @Volatile
    private var revokedByOtherVpn = false

    /** One-shot user-facing messages (errors / confirmations) for snackbars. */
    private val _toasts = MutableSharedFlow<String>(extraBufferCapacity = 8)
    val toasts: SharedFlow<String> = _toasts.asSharedFlow()

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    @Volatile
    private var engine: MobileEngine? = null

    /** The IP/MTU the engine wants a tunnel for, pending VPN consent. */
    @Volatile
    private var pendingTun: Pair<String, Int>? = null

    data class PendingJoin(val nodeId: String, val hostname: String, val sas: List<String>)

    /** App-private internal storage for `node.key`, `network.cbor`, `docs/`, `blobs/`,
     * `secrets/`. Not world-readable, so file-backed secrets are acceptable here. */
    fun dataDir(context: Context): File =
        File(context.filesDir, "engine").apply { mkdirs() }

    /**
     * A stable, user-uneditable display name other members see. The Android OS
     * hostname is meaningless ("localhost"), and the hardware serial is
     * unreachable to normal apps, so we derive `"<Manufacturer> <Model> (<suffix>)"`
     * where `<suffix>` disambiguates identical models via Settings.Secure.ANDROID_ID.
     * Not tamper-proof (no name field is, on any platform) — the NodeId is the real
     * identity anchor.
     */
    @Suppress("HardwareIds")
    fun deviceName(context: Context): String {
        val manufacturer = Build.MANUFACTURER?.replaceFirstChar { it.uppercase() }.orEmpty().trim()
        val model = Build.MODEL.orEmpty().trim()
        val base = when {
            model.isEmpty() -> manufacturer.ifEmpty { "Android device" }
            manufacturer.isEmpty() || model.startsWith(manufacturer, ignoreCase = true) -> model
            else -> "$manufacturer $model"
        }
        val androidId = runCatching {
            Settings.Secure.getString(context.contentResolver, Settings.Secure.ANDROID_ID)
        }.getOrNull().orEmpty()
        val suffix = if (androidId.length >= 4) androidId.takeLast(4) else "0000"
        return "$base ($suffix)"
    }

    /** Idempotent: boot the engine once and seed the initial UI state. */
    @Synchronized
    fun start(context: Context) {
        if (engine != null) return
        val appCtx = context.applicationContext
        val data = dataDir(appCtx).absolutePath
        val name = deviceName(appCtx)
        Log.i(TAG, "starting engine data=$data name=$name")
        val listener = object : EventListener {
            override fun onChanged() = refreshStatus()
            override fun onJoinSas(sas: List<String>) {
                _joinSas.value = sas
            }

            override fun onJoinRequest(nodeId: String, hostname: String, sas: List<String>) {
                _pendingJoins.value = (_pendingJoins.value.filterNot { it.nodeId == nodeId } +
                    PendingJoin(nodeId, hostname, sas))
            }

            override fun onTunSetupRequired(ip: String, mtu: UInt) {
                onTunSetup(appCtx, ip, mtu.toInt())
            }

            override fun onTunTeardownRequired() {
                // The engine has already dropped its TUN (closing the fd, which
                // tears down the interface). Nothing to do but refresh the UI.
                refreshStatus()
            }
        }
        try {
            engine = MobileEngine.init(data, name, listener)
            _running.value = true
            refreshStatus()
        } catch (e: Exception) {
            Log.e(TAG, "engine init failed", e)
            emit("Engine failed to start: ${e.message}")
        }
    }

    @Synchronized
    fun stop() {
        runCatching { engine?.shutdown() }
        runCatching { engine?.close() }
        engine = null
        pendingTun = null
        _needsVpnConsent.value = false
        _running.value = false
        _status.value = null
    }

    fun isStarted(): Boolean = engine != null

    fun assignedIp(): String? = engine?.assignedIp()

    // --- visibility / pace / connectivity ---------------------------------

    /** MainActivity resumed/paused. Foreground (with the screen on) runs the engine
     * at its interactive cadence; backgrounding drops it to the battery-saving one. */
    fun setActivityResumed(resumed: Boolean) {
        activityResumed = resumed
        applyPace()
    }

    /** Screen turned interactive (on) or not (off) — the other half of "foreground". */
    fun setScreenInteractive(on: Boolean) {
        screenInteractive = on
        applyPace()
    }

    private fun applyPace() {
        val interactive = activityResumed && screenInteractive
        _foreground.value = interactive
        // setPace is a cheap atomic store + wakeup on the engine; safe off any thread.
        runCatching { engine?.setPace(background = !interactive) }
    }

    /**
     * The device's connectivity changed (from [NetworkMonitor]). If a foreign VPN had
     * forced us offline and the user still wants to be online, resume as soon as it's
     * gone (`VpnService.prepare == null` — no other app owns the VPN); either way,
     * hint the engine so iroh rebinds its sockets and re-forms the mesh.
     */
    fun onNetworkChanged(context: Context) {
        scope.launch {
            if (revokedByOtherVpn && userWantsOnline &&
                VpnService.prepare(context) == null
            ) {
                revokedByOtherVpn = false
                // setOnline re-activates the network; the engine then re-emits
                // TunSetupRequired, which brings the VpnService tunnel back up.
                runCatching { requireEngine().setOnline(true) }
                    .onFailure { Log.w(TAG, "resume after revoke failed", it) }
            }
            runCatching { engine?.networkChanged() }
        }
    }

    /**
     * Another VPN took over routing (our tunnel was revoked). Quiesce fully: while a
     * foreign VPN owns the network our data plane is dead, so keeping the mesh alive
     * is pure battery burn and would show peers a green dot for a phone they can't
     * reach. We auto-resume from [onNetworkChanged] once it's gone (gated on
     * [userWantsOnline], so a deliberate offline is respected).
     */
    fun onVpnRevoked() {
        revokedByOtherVpn = true
        // Raw engine call (not the wrapper): this is a forced offline, so it must not
        // clear userWantsOnline or we'd never auto-resume.
        scope.launch { runCatching { engine?.setOnline(false) } }
    }

    private fun refreshStatus() {
        scope.launch {
            val s = runCatching { engine?.status() }.getOrNull()
            _status.value = s
        }
    }

    // --- VPN / TUN coordination -------------------------------------------

    /** Engine asked for a tunnel. Establish immediately if VPN consent is already
     * granted; otherwise stash it and surface [needsVpnConsent] for the UI. */
    private fun onTunSetup(context: Context, ip: String, mtu: Int) {
        pendingTun = ip to mtu
        if (VpnService.prepare(context) == null) {
            _needsVpnConsent.value = false
            NullgateVpnService.establishTun(context, ip, mtu)
        } else {
            _needsVpnConsent.value = true
            emit("Tap “Enable routing” to grant the VPN permission")
        }
    }

    /** Called after the user grants the system VPN consent dialog. */
    fun onVpnConsentGranted(context: Context) {
        _needsVpnConsent.value = false
        val (ip, mtu) = pendingTun ?: (assignedIp()?.let { it to TUN_MTU }) ?: return
        NullgateVpnService.establishTun(context, ip, mtu)
    }

    /** Hand the engine the TUN fd from the VPN service (ownership transfers to Rust). */
    fun attachTun(fd: Int) {
        scope.launch {
            runCatching { engine?.attachTun(fd) }
                .onFailure { emit("Routing failed: ${it.message}") }
        }
    }

    /** Drop the engine's TUN (VPN revoked / explicit teardown). */
    fun detachTun() {
        scope.launch { runCatching { engine?.detachTun() } }
    }

    // --- mutations (suspend; run off the main thread) ---------------------

    suspend fun createNetwork(name: String): String = io {
        userWantsOnline = true
        requireEngine().createNetwork(name)
    }

    suspend fun joinNetwork(ticket: String) = io {
        userWantsOnline = true
        _joinSas.value = null
        requireEngine().joinNetwork(ticket)
    }

    suspend fun leaveNetwork() = io {
        userWantsOnline = false
        requireEngine().leaveNetwork()
    }

    suspend fun setOnline(online: Boolean) = io {
        userWantsOnline = online
        requireEngine().setOnline(online)
    }
    suspend fun deleteNetwork() = io { requireEngine().deleteNetwork() }
    suspend fun rotateNetwork(): String = io { requireEngine().rotateNetwork() }
    suspend fun setNetworkName(name: String) = io { requireEngine().setNetworkName(name) }
    suspend fun setFrozen(frozen: Boolean) = io { requireEngine().setFrozen(frozen) }

    suspend fun approveJoin(nodeId: String) = io {
        requireEngine().approveJoin(nodeId)
        clearPending(nodeId)
    }

    suspend fun denyJoin(nodeId: String) = io {
        requireEngine().denyJoin(nodeId)
        clearPending(nodeId)
    }

    suspend fun removeMember(nodeId: String) = io { requireEngine().removeMember(nodeId) }
    suspend fun setMemberRole(nodeId: String, controller: Boolean) =
        io { requireEngine().setMemberRole(nodeId, controller) }

    suspend fun ticket(): String = io { requireEngine().ticket() }
    suspend fun controllerTicket(): String = io { requireEngine().controllerTicket() }
    suspend fun setPeerTicketSingleUse(on: Boolean) =
        io { requireEngine().setPeerTicketSingleUse(on) }

    suspend fun exportOriginatorKey(): String = io { requireEngine().exportOriginatorKey() }
    suspend fun importOriginatorKey(code: String) = io { requireEngine().importOriginatorKey(code) }

    suspend fun setRemoteAccessDisabled(disabled: Boolean) =
        io { requireEngine().setRemoteAccessDisabled(disabled) }

    suspend fun setHidden(hidden: Boolean) = io { requireEngine().setHidden(hidden) }
    suspend fun setNickname(nodeId: String, name: String?) =
        io { requireEngine().setNickname(nodeId, name) }

    suspend fun setNote(nodeId: String, note: String?) =
        io { requireEngine().setNote(nodeId, note) }

    suspend fun auditLog(): List<AuditEntry> = io { requireEngine().auditLog() }

    // --- custom relay servers ---------------------------------------------
    //
    // Per-device settings (`relays.cbor` in the app data dir), exactly like the
    // desktop — they are NOT distributed through the roster, so the same relays
    // and tokens have to be set on every member or the ones missing them cannot
    // be reached.

    suspend fun relayStatus(): RelayStatus = io { requireEngine().relayStatus() }

    /** Returns once saved; the engine pushes it into the live endpoint behind
     * that, so re-read [relayStatus] for the verdict rather than assuming this
     * means "applied". */
    suspend fun setRelaySettings(servers: List<RelayServer>, mode: RelayPolicy) =
        io { requireEngine().setRelaySettings(servers, mode) }

    fun members(): List<MemberView> = _status.value?.members.orEmpty()

    /** Dismiss the "verify these emojis" banner shown while we are joining. */
    fun clearJoinSas() {
        _joinSas.value = null
    }

    // --- helpers ----------------------------------------------------------

    private fun clearPending(nodeId: String) {
        _pendingJoins.value = _pendingJoins.value.filterNot { it.nodeId == nodeId }
    }

    private suspend fun <T> io(block: suspend () -> T): T = withContext(Dispatchers.IO) { block() }

    private fun emit(msg: String) {
        _toasts.tryEmit(msg)
    }

    private fun requireEngine(): MobileEngine = engine ?: error("engine not started")
}
