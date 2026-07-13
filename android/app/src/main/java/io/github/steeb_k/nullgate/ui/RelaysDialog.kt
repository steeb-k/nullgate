package io.github.steeb_k.nullgate.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import io.github.steeb_k.nullgate.engine.EngineHolder
import kotlinx.coroutines.launch
import uniffi.ipn_mobile.RelayApply
import uniffi.ipn_mobile.RelayPolicy
import uniffi.ipn_mobile.RelayServer
import uniffi.ipn_mobile.RelayStatus

/**
 * Custom relay servers for *this device*: list, add (URL + optional token),
 * remove, and the keep-the-public-relays policy. Mirrors the desktop's "Relay
 * servers" flyout, warning included.
 *
 * The warning is the point of this screen. Relay settings are per-device and are
 * not distributed through the roster, so a self-hosted relay configured on only
 * some members splits the network: the configured ones advertise it as their home
 * relay and a peer without it (or without its token) has no path to them. The
 * phone had no relay surface at all before this, which is the only reason it
 * stayed reachable through the July 2026 partition.
 */
@Composable
fun RelaysDialog(onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var status by remember { mutableStateOf<RelayStatus?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var adding by remember { mutableStateOf(false) }

    suspend fun reload() {
        runCatching { EngineHolder.relayStatus() }
            .onSuccess { status = it; error = null }
            .onFailure { error = it.message }
    }
    LaunchedEffect(Unit) { reload() }

    // Save, then re-read so the dialog shows what the engine actually reached
    // rather than what we asked for.
    fun save(servers: List<RelayServer>, mode: RelayPolicy) {
        scope.launch {
            runCatching { EngineHolder.setRelaySettings(servers, mode) }
                .onFailure { error = it.message }
            reload()
        }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Relay servers") },
        text = {
            val st = status
            Column(Modifier.heightIn(max = 460.dp).verticalScroll(rememberScrollState())) {
                error?.let {
                    Text(it, style = MaterialTheme.typography.bodySmall)
                    Spacer(Modifier.size(8.dp))
                }
                if (st == null) {
                    Text("Loading…")
                    return@Column
                }

                if (st.servers.isEmpty()) {
                    Text(
                        "Nullgate uses the public iroh relays to help devices find each " +
                            "other. Add your own relay to carry that traffic instead.",
                        style = MaterialTheme.typography.bodyMedium,
                    )
                } else {
                    WarningCard(
                        "Set these same relays — same address and token — on every device. " +
                            "A device without them cannot be reached by the ones that have them."
                    )
                    Spacer(Modifier.size(12.dp))
                }

                ApplyState(st.apply)

                st.servers.forEach { server ->
                    Row(
                        Modifier.fillMaxWidth().padding(vertical = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                server.url,
                                style = MaterialTheme.typography.bodyMedium,
                                fontFamily = FontFamily.Monospace,
                            )
                            Text(
                                if (server.token != null) "Access token set" else "No access token",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                        TextButton(onClick = {
                            save(st.servers.filterNot { it.url == server.url }, st.mode)
                        }) { Text("Remove") }
                    }
                    HorizontalDivider()
                }

                TextButton(onClick = { adding = true }) { Text("Add a relay server") }

                // The policy only means anything once a custom relay exists.
                if (st.servers.isNotEmpty()) {
                    Spacer(Modifier.size(12.dp))
                    Text("Public iroh relays", style = MaterialTheme.typography.titleSmall)
                    PolicyChoice(
                        selected = st.mode,
                        onSelect = { save(st.servers, it) },
                    )
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )

    if (adding) {
        val st = status
        AddRelayDialog(
            onDismiss = { adding = false },
            onAdd = { url, token ->
                adding = false
                val servers = st?.servers.orEmpty().filterNot { it.url == url } +
                    RelayServer(url = url, token = token)
                save(servers, st?.mode ?: RelayPolicy.PREFERRED)
            },
        )
    }
}

/** How far the last change got in reaching the running engine. */
@Composable
private fun ApplyState(apply: RelayApply) {
    when (apply) {
        is RelayApply.Applied -> Unit
        is RelayApply.Pending -> {
            Text("Saved; still applying…", style = MaterialTheme.typography.bodySmall)
            Spacer(Modifier.size(8.dp))
        }
        is RelayApply.Failed -> {
            WarningCard("Saved, but not applied — restart Nullgate. (${apply.reason})")
            Spacer(Modifier.size(8.dp))
        }
    }
}

@Composable
private fun PolicyChoice(selected: RelayPolicy, onSelect: (RelayPolicy) -> Unit) {
    val options = listOf(
        RelayPolicy.PREFERRED to
            ("Keep as a backup (recommended)" to
                "Your relay carries the traffic, but devices without it can still reach you."),
        RelayPolicy.ONLY to
            ("Never use them" to
                "Only your relays. Devices without them cannot reach you at all."),
    )
    options.forEach { (mode, text) ->
        val (title, subtitle) = text
        Row(
            Modifier.fillMaxWidth().padding(vertical = 4.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            RadioButton(selected = selected == mode, onClick = { onSelect(mode) })
            Column(Modifier.weight(1f)) {
                Text(title, style = MaterialTheme.typography.bodyMedium)
                Text(subtitle, style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

@Composable
private fun AddRelayDialog(onDismiss: () -> Unit, onAdd: (String, String?) -> Unit) {
    var url by remember { mutableStateOf("") }
    var token by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add a relay server") },
        text = {
            Column {
                Text(
                    "The address of your own iroh relay. If it requires an access token, " +
                        "paste it below.",
                    style = MaterialTheme.typography.bodySmall,
                )
                Spacer(Modifier.size(8.dp))
                OutlinedTextField(
                    value = url,
                    onValueChange = { url = it },
                    label = { Text("https://relay.example.com:8443") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.size(8.dp))
                OutlinedTextField(
                    value = token,
                    onValueChange = { token = it },
                    label = { Text("Access token (optional)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(enabled = url.isNotBlank(), onClick = {
                onAdd(url.trim(), token.trim().ifBlank { null })
            }) { Text("Add") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun WarningCard(text: String) {
    Card(
        Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        ),
    ) {
        Text(text, Modifier.padding(12.dp), style = MaterialTheme.typography.bodySmall)
    }
}
