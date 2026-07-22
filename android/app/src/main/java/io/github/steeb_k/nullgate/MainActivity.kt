package io.github.steeb_k.nullgate

import android.Manifest
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import io.github.steeb_k.nullgate.engine.EngineHolder
import io.github.steeb_k.nullgate.engine.NullgateVpnService
import io.github.steeb_k.nullgate.ui.AppScreen
import io.github.steeb_k.nullgate.ui.NullgateTheme

class MainActivity : ComponentActivity() {

    private var pendingScan: ((String?) -> Unit)? = null

    private val scanLauncher = registerForActivityResult(ScanContract()) { res ->
        val cb = pendingScan
        pendingScan = null
        cb?.invoke(res.contents)
    }

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* best effort */ }

    private val vpnConsent =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { res ->
            if (res.resultCode == RESULT_OK) {
                EngineHolder.onVpnConsentGranted(applicationContext)
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }

        // Boot the engine via the foreground service (idempotent).
        NullgateVpnService.start(applicationContext)

        setContent {
            NullgateTheme {
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    AppScreen(
                        onScanTicket = ::scanTicket,
                        onEnableRouting = ::requestVpnConsent,
                    )
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        // Foreground: run the engine at its interactive cadence (see EngineHolder).
        EngineHolder.setActivityResumed(true)
    }

    override fun onPause() {
        super.onPause()
        EngineHolder.setActivityResumed(false)
    }

    private fun scanTicket(onResult: (String?) -> Unit) {
        pendingScan = onResult
        val opts = ScanOptions()
            .setOrientationLocked(false)
            .setCaptureActivity(PortraitCaptureActivity::class.java)
            .setBeepEnabled(false)
            .setPrompt("Scan a Nullgate join ticket")
        scanLauncher.launch(opts)
    }

    /** Show the system VPN consent dialog if needed, then establish routing. */
    private fun requestVpnConsent() {
        val intent = VpnService.prepare(this)
        if (intent != null) {
            vpnConsent.launch(intent)
        } else {
            EngineHolder.onVpnConsentGranted(applicationContext)
        }
    }
}
