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
 *  - the **default network**: transitions of its identity, transports, or
 *    **validated** state (Android's own "this network actually reaches the
 *    internet" probe — the one signal that fires when a network stays up but
 *    stops working) trigger a debounced [onNetworkChanged]. Our *own* VPN
 *    becoming the default is deliberately ignored ([ourTunnelActive]): the
 *    tunnel's lifecycle used to self-trigger a full recovery burst per
 *    establish/teardown. A *foreign* VPN still registers, which is what drives
 *    the auto-resume after it releases the network.
 *  - the **non-VPN internet** networks (all of them, not just the default): kept
 *    as an ordered set and reported as the VpnService's underlying networks. The
 *    old single-network version was last-callback-wins, which could report
 *    cellular as the underlying network while traffic actually rode Wi-Fi —
 *    making Android bill Wi-Fi bytes to the mobile-data counter.
 */
class NetworkMonitor(
    context: Context,
    private val onNetworkChanged: () -> Unit,
    private val onUnderlyingNetworks: (Array<Network>) -> Unit,
    private val ourTunnelActive: () -> Boolean,
) {
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var debounceJob: Job? = null

    private var defaultCb: ConnectivityManager.NetworkCallback? = null
    private var internetCb: ConnectivityManager.NetworkCallback? = null

    /** Fingerprint of the default network: identity + transports + validated. Only a
     * *change* triggers — `onCapabilitiesChanged` fires constantly for bandwidth/
     * signal estimates, and a rebind per RSSI tick would defeat the battery win. */
    @Volatile
    private var lastDefault = ""

    /** Non-VPN internet networks and their latest capabilities. Guarded by [lock]
     * (callbacks arrive on ConnectivityManager's thread). */
    private val lock = Any()
    private val internets = LinkedHashMap<Network, NetworkCapabilities>()
    private var lastUnderlying: List<Network> = emptyList()

    /** Identity + validated state of the best underlying network. While our own VPN
     * is the default (and filtered out of [onDefaultState]), this is what still
     * fires the recovery hint on a Wi-Fi↔cellular switch underneath the tunnel. */
    private var lastPrimary = ""

    fun start() {
        if (cm == null) {
            Log.w(TAG, "no ConnectivityManager; network-change recovery disabled")
            return
        }
        val defCb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) =
                onDefaultState(network, cm.getNetworkCapabilities(network))

            override fun onLost(network: Network) = onDefaultState(null, null)

            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) =
                onDefaultState(network, caps)
        }
        runCatching { cm.registerDefaultNetworkCallback(defCb) }
            .onSuccess { defaultCb = defCb }
            .onFailure { Log.w(TAG, "registerDefaultNetworkCallback failed", it) }

        val req = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
        val netCb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                cm.getNetworkCapabilities(network)?.let { update(network, it) }
            }

            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) =
                update(network, caps)

            override fun onLost(network: Network) = remove(network)
        }
        runCatching { cm.registerNetworkCallback(req, netCb) }
            .onSuccess { internetCb = netCb }
            .onFailure { Log.w(TAG, "registerNetworkCallback failed", it) }
    }

    // --- recovery trigger (default network) --------------------------------

    private fun onDefaultState(network: Network?, caps: NetworkCapabilities?) {
        // Our own tunnel becoming (or ceasing to be) the default is not a
        // connectivity change — reacting to it made every establish/teardown
        // cost a full recovery burst.
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true && ourTunnelActive()) {
            return
        }
        val transports = TRANSPORTS.fold(0) { mask, t ->
            if (caps?.hasTransport(t) == true) mask or (1 shl t) else mask
        }
        val validated =
            caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
        val fp = if (network == null) "none" else "$network/$transports/$validated"
        if (fp != lastDefault) {
            lastDefault = fp
            trigger()
        }
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

    // --- underlying networks (non-VPN internet set) ------------------------

    private fun update(network: Network, caps: NetworkCapabilities) {
        synchronized(lock) {
            internets[network] = caps
            pushUnderlying()
        }
    }

    private fun remove(network: Network) {
        synchronized(lock) {
            internets.remove(network)
            pushUnderlying()
        }
    }

    /** Report the full ordered set (validated first, then by transport quality) and
     * only when the *order* changed — capabilities churn constantly. An empty array
     * is accurate when no non-VPN internet exists; `null` ("follow the default")
     * is never passed, because under a VPN the default *is* the VPN. */
    private fun pushUnderlying() {
        val ordered = internets.entries.sortedByDescending { score(it.value) }.map { it.key }
        if (ordered != lastUnderlying) {
            lastUnderlying = ordered
            onUnderlyingNetworks(ordered.toTypedArray())
        }
        val primary = ordered.firstOrNull()
        val primaryFp = primary?.let {
            val validated = internets[it]
                ?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
            "$it/$validated"
        } ?: "none"
        if (primaryFp != lastPrimary) {
            lastPrimary = primaryFp
            trigger()
        }
    }

    private fun score(c: NetworkCapabilities): Int {
        var s = 0
        if (c.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)) s += 100
        if (c.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)) s += 50
        if (c.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) s += 40
        if (c.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)) s += 20
        if (c.hasTransport(NetworkCapabilities.TRANSPORT_BLUETOOTH)) s += 10
        return s
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

        /** Transports whose presence/absence is a real path change worth reacting to.
         * TRANSPORT_VPN is intentionally absent: our own tunnel is filtered above,
         * and a foreign VPN still changes the fingerprint via the network identity. */
        val TRANSPORTS = intArrayOf(
            NetworkCapabilities.TRANSPORT_WIFI,
            NetworkCapabilities.TRANSPORT_CELLULAR,
            NetworkCapabilities.TRANSPORT_ETHERNET,
            NetworkCapabilities.TRANSPORT_BLUETOOTH,
        )
    }
}
