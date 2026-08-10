package io.github.steeb_k.nullgate.engine

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.SystemClock
import android.util.Log

/**
 * The level-triggered half of connectivity recovery. Everything else is
 * edge-triggered on `ConnectivityManager` callbacks — and the killer failure
 * (post-doze NAT/socket death) produces *no* edge: the network looks unchanged
 * while every iroh path is dead. This alarm fires in doze maintenance windows
 * (`setAndAllowWhileIdle`), re-arms itself (the API is one-shot by design), makes
 * sure the service+engine exist at all (recovering a failed boot-time init), and
 * runs the engine's health check, which escalates burst → full node rebuild.
 * Soak baseline without this: 12/12 blackhole cycles never recovered; a rebuild
 * recovers in ~5 s.
 */
class HealthCheckReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        schedule(context)
        // Idempotent; also the retry path for an engine whose init failed (e.g.
        // Always-on VPN started us at boot before the network was up).
        NullgateVpnService.start(context)
        EngineHolder.healthCheck(context)
    }

    companion object {
        private const val TAG = "HealthCheck"
        private const val INTERVAL_MS = 15L * 60 * 1000

        /** Cadence while UNHEALTHY: the recovery ladder (burst → rebuild →
         * process restart) climbs one rung per check, so the check interval is
         * the ladder's pacing. At the healthy 15-min cadence (stretched to ~26
         * min by deep-doze batching) full escalation took 50-80 min — soak
         * cycles recovered at ~36-38 min only when the alarm phase got lucky.
         * 3 min compresses the ladder to ~10-15 min worst case; deep doze may
         * stretch it to ~9 min per fire, still 3× faster. Costs nothing when
         * healthy — the fast reschedule happens only after a bad check. */
        private const val UNHEALTHY_INTERVAL_MS = 3L * 60 * 1000
        private const val REQUEST_CODE = 1001

        private fun pending(context: Context): PendingIntent =
            PendingIntent.getBroadcast(
                context,
                REQUEST_CODE,
                Intent(context, HealthCheckReceiver::class.java),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )

        /** (Re-)arm the next check. `setAndAllowWhileIdle` needs no permission and
         * is deliberately inexact — the system batches it, which is fine here.
         * One PendingIntent: re-scheduling REPLACES the pending alarm, so the
         * fast path pulls the same chain in rather than forking a second one. */
        fun schedule(context: Context, fast: Boolean = false) {
            val am = context.getSystemService(AlarmManager::class.java) ?: return
            val delay = if (fast) UNHEALTHY_INTERVAL_MS else INTERVAL_MS
            runCatching {
                am.setAndAllowWhileIdle(
                    AlarmManager.ELAPSED_REALTIME_WAKEUP,
                    SystemClock.elapsedRealtime() + delay,
                    pending(context),
                )
            }.onFailure { Log.w(TAG, "could not schedule health check", it) }
        }

        fun cancel(context: Context) {
            val am = context.getSystemService(AlarmManager::class.java) ?: return
            runCatching { am.cancel(pending(context)) }
        }
    }
}
