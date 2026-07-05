package io.github.steeb_k.nullgate.engine

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.VpnService
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import io.github.steeb_k.nullgate.MainActivity
import io.github.steeb_k.nullgate.R
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import uniffi.ipn_mobile.NetworkStatus

/**
 * The foreground service that hosts the engine for the life of the process — the
 * Android equivalent of the desktop daemon — *and* the [VpnService] that owns the
 * TUN interface. Routing only ever flows through a system-granted VPN, so folding
 * both jobs into one component keeps the lifecycle simple: while this service runs,
 * the engine runs; the tunnel comes and goes with the engine's online state via
 * [EngineHolder]'s setup/teardown events.
 *
 * Split tunnel: we only `addRoute(10.99.0.0/24)`, so the phone's normal traffic
 * (and iroh's own relay/peer traffic to public IPs) bypasses the tunnel — no
 * `protect()` needed.
 */
class NullgateVpnService : VpnService() {

    private var started = false
    private var multicastLock: WifiManager.MulticastLock? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_ESTABLISH -> {
                ensureStarted()
                val ip = intent.getStringExtra(EXTRA_IP)
                val mtu = intent.getIntExtra(EXTRA_MTU, EngineHolder.TUN_MTU)
                if (ip != null) establishTun(ip, mtu)
            }
            ACTION_STOP -> {
                stopRouting()
                stopForegroundCompat()
                stopSelf()
            }
            else -> ensureStarted()
        }
        return START_STICKY
    }

    /** One-time engine startup + notification refresh loop. */
    private fun ensureStarted() {
        if (started) return
        started = true
        startForegroundCompat(buildNotification("Starting…"))
        acquireMulticastLock()
        EngineHolder.start(applicationContext)

        // Keep the notification subtitle live from the engine status.
        scope.launch {
            EngineHolder.status.collectLatest { st ->
                notificationManager().notify(NOTIF_ID, buildNotification(stateText(st)))
            }
        }
    }

    /** Build the VPN interface for our virtual IP and hand its fd to the engine.
     * Consent must already be granted (the caller checks `VpnService.prepare`). */
    private fun establishTun(ip: String, mtu: Int) {
        runCatching {
            val builder = Builder()
                .setSession("Nullgate")
                .addAddress(ip, 24)
                .addRoute(VIRTUAL_SUBNET, 24)
                .setMtu(mtu)
            // Don't route DNS or other traffic — split tunnel for the /24 only.
            val pfd = builder.establish()
                ?: error("VpnService.establish() returned null (permission revoked?)")
            // Ownership of the fd transfers to Rust (detachFd); it closes it on drop.
            EngineHolder.attachTun(pfd.detachFd())
            Log.i(TAG, "VPN established at $ip/24 (mtu $mtu)")
        }.onFailure { Log.e(TAG, "establishTun failed", it) }
    }

    /** Drop routing without stopping the engine (the engine clears its own TUN; this
     * is here for an explicit teardown / onRevoke). */
    private fun stopRouting() {
        EngineHolder.detachTun()
    }

    /** The system or user revoked the VPN (e.g. another VPN took over). */
    override fun onRevoke() {
        Log.i(TAG, "VPN revoked by system/user")
        stopRouting()
        super.onRevoke()
    }

    override fun onDestroy() {
        releaseMulticastLock()
        scope.cancel()
        EngineHolder.stop()
        super.onDestroy()
    }

    // --- notification ------------------------------------------------------

    private fun stateText(st: NetworkStatus?): String = when {
        st == null -> "Not connected to a network"
        !st.online -> "Disconnected · ${st.name}"
        st.routing -> {
            val online = st.members.count { it.online && !it.isSelf }
            "Connected · ${st.name} · $online peer(s) online"
        }
        else -> "Connected · ${st.name} · routing off"
    }

    private fun buildNotification(stateText: String): Notification {
        val tap = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(stateText)
            .setSmallIcon(R.drawable.ic_stat_nullgate)
            .setOngoing(true)
            .setContentIntent(tap)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    private fun startForegroundCompat(n: Notification) {
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIF_ID, n)
        }
    }

    private fun stopForegroundCompat() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            stopForeground(STOP_FOREGROUND_REMOVE)
        } else {
            @Suppress("DEPRECATION")
            stopForeground(true)
        }
    }

    private fun acquireMulticastLock() {
        if (multicastLock != null) return
        runCatching {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            wifi.createMulticastLock("nullgate.mdns").apply {
                setReferenceCounted(false)
                acquire()
                multicastLock = this
            }
        }.onFailure { Log.w(TAG, "could not acquire multicast lock for LAN discovery", it) }
    }

    private fun releaseMulticastLock() {
        multicastLock?.let { lock -> runCatching { if (lock.isHeld) lock.release() } }
        multicastLock = null
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.notif_channel_net),
                NotificationManager.IMPORTANCE_LOW
            )
            notificationManager().createNotificationChannel(ch)
        }
    }

    private fun notificationManager() =
        getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    companion object {
        private const val TAG = "NullgateVpn"
        private const val CHANNEL_ID = "nullgate.net"
        private const val NOTIF_ID = 1
        private const val VIRTUAL_SUBNET = "10.99.0.0"
        const val ACTION_ESTABLISH = "io.github.steeb_k.nullgate.action.ESTABLISH"
        const val ACTION_STOP = "io.github.steeb_k.nullgate.action.STOP"
        const val EXTRA_IP = "ip"
        const val EXTRA_MTU = "mtu"

        /** Start (or ensure) the service hosting the engine. */
        fun start(context: Context) {
            val i = Intent(context, NullgateVpnService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(i)
            } else {
                context.startService(i)
            }
        }

        /** Ask the service to build the TUN for `ip`/24 (consent already granted). */
        fun establishTun(context: Context, ip: String, mtu: Int) {
            val i = Intent(context, NullgateVpnService::class.java)
                .setAction(ACTION_ESTABLISH)
                .putExtra(EXTRA_IP, ip)
                .putExtra(EXTRA_MTU, mtu)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(i)
            } else {
                context.startService(i)
            }
        }

        /** Stop the service entirely (and thus the engine). */
        fun stop(context: Context) {
            context.startService(
                Intent(context, NullgateVpnService::class.java).setAction(ACTION_STOP)
            )
        }
    }
}
