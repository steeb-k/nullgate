package io.github.steeb_k.nullgate.engine

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * Watches the device's connectivity and feeds the engine the network-change signal
 * iroh cannot obtain for itself on Android (its native network monitor is a no-op
 * there). Without this, the iroh endpoint keeps stale sockets/paths/relay
 * connections across a network switch and peers stay invisible until the whole VPN
 * is toggled — the exact bug this fixes.
 *
 * Two callbacks:
 *  - the **default network** (any transport, incl. VPN): any transition — Wi-Fi↔
 *    cellular, airplane mode, a foreign VPN coming up or going away — triggers a
 *    debounced [onNetworkChanged]. The debounce collapses the burst of callbacks a
 *    single switch emits, and a spurious call is harmless (iroh documents the hint
 *    as safe to call at any time).
 *  - the **non-VPN internet** network: while our tunnel is up we report it as the
 *    VpnService's underlying network, so the system's transport/metering view of our
 *    VPN stays correct across Wi-Fi↔cellular.
 */
class NetworkMonitor(
    context: Context,
    private val onNetworkChanged: () -> Unit,
    private val onUnderlyingNetwork: (Network?) -> Unit,
) {
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var debounceJob: Job? = null

    private var defaultCb: ConnectivityManager.NetworkCallback? = null
    private var internetCb: ConnectivityManager.NetworkCallback? = null

    /** Transport bitmask of the current default network. `onCapabilitiesChanged`
     * fires constantly (bandwidth/signal estimates); we only care when the *transport*
     * changes (Wi-Fi↔cellular, a VPN coming/going), not on every RSSI tick — a
     * needless rebind on signal noise would defeat the battery win. */
    @Volatile
    private var lastTransports = -1

    fun start() {
        if (cm == null) {
            Log.w(TAG, "no ConnectivityManager; network-change recovery disabled")
            return
        }
        val defCb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) = trigger()
            override fun onLost(network: Network) {
                lastTransports = -1
                trigger()
            }

            override fun onCapabilitiesChanged(
                network: Network,
                caps: NetworkCapabilities,
            ) {
                val transports = TRANSPORTS.fold(0) { mask, t ->
                    if (caps.hasTransport(t)) mask or (1 shl t) else mask
                }
                if (transports != lastTransports) {
                    lastTransports = transports
                    trigger()
                }
            }
        }
        runCatching { cm.registerDefaultNetworkCallback(defCb) }
            .onSuccess { defaultCb = defCb }
            .onFailure { Log.w(TAG, "registerDefaultNetworkCallback failed", it) }

        val req = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
        val netCb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) = onUnderlyingNetwork(network)
            override fun onLost(network: Network) = onUnderlyingNetwork(null)
        }
        runCatching { cm.registerNetworkCallback(req, netCb) }
            .onSuccess { internetCb = netCb }
            .onFailure { Log.w(TAG, "registerNetworkCallback failed", it) }
    }

    /** Debounce: one settle window per burst of transitions. The default callback
     * also fires once on registration — the delay absorbs that startup call. */
    private fun trigger() {
        debounceJob?.cancel()
        debounceJob = scope.launch {
            delay(DEBOUNCE_MS)
            onNetworkChanged()
        }
    }

    fun stop() {
        defaultCb?.let { cb -> runCatching { cm?.unregisterNetworkCallback(cb) } }
        internetCb?.let { cb -> runCatching { cm?.unregisterNetworkCallback(cb) } }
        defaultCb = null
        internetCb = null
        scope.cancel()
    }

    private companion object {
        const val TAG = "NetworkMonitor"
        const val DEBOUNCE_MS = 1_500L

        /** Transports whose presence/absence is a real path change worth reacting to. */
        val TRANSPORTS = intArrayOf(
            NetworkCapabilities.TRANSPORT_WIFI,
            NetworkCapabilities.TRANSPORT_CELLULAR,
            NetworkCapabilities.TRANSPORT_ETHERNET,
            NetworkCapabilities.TRANSPORT_VPN,
            NetworkCapabilities.TRANSPORT_BLUETOOTH,
        )
    }
}
