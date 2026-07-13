package io.github.steeb_k.nullgate.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.Card
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.runtime.collectAsState
import io.github.steeb_k.nullgate.engine.EngineHolder
import kotlinx.coroutines.launch
import uniffi.ipn_mobile.MemberView
import uniffi.ipn_mobile.NetworkStatus

/**
 * The single top-level screen. It renders the current [NetworkStatus] (header +
 * member list), pending join approvals, and routes every administrative action
 * through dialogs (see Dialogs.kt). All engine calls run off the main thread via
 * [EngineHolder]; failures surface as snackbars.
 *
 * @param onScanTicket launches the QR scanner; the callback receives the scanned
 *   ticket text (or null if cancelled).
 * @param onEnableRouting triggers the system VPN consent flow so routing can come up.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppScreen(
    onScanTicket: ((String?) -> Unit) -> Unit,
    onEnableRouting: () -> Unit,
) {
    val status by EngineHolder.status.collectAsState()
    val pendingJoins by EngineHolder.pendingJoins.collectAsState()
    val joinSas by EngineHolder.joinSas.collectAsState()
    val needsConsent by EngineHolder.needsVpnConsent.collectAsState()

    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val runAction: (suspend () -> Unit) -> Unit = { block ->
        scope.launch {
            runCatching { block() }.onFailure { snackbar.showSnackbar(it.message ?: "Error") }
        }
    }

    // Surface engine-side messages (errors / hints) as snackbars.
    androidx.compose.runtime.LaunchedEffect(Unit) {
        EngineHolder.toasts.collect { snackbar.showSnackbar(it) }
    }

    var dialog by remember { mutableStateOf<Dialog?>(null) }
    var menuOpen by remember { mutableStateOf(false) }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            TopAppBar(
                title = { Text("Nullgate") },
                actions = {
                    // Shown even with no network: relay settings are device-level,
                    // and a token-gated relay may be what this device needs in
                    // order to reach the network it is about to join.
                    IconButton(onClick = { menuOpen = true }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "Menu")
                    }
                    NetworkMenu(
                        expanded = menuOpen,
                        status = status,
                        onDismiss = { menuOpen = false },
                        onAction = { dialog = it; menuOpen = false },
                    )
                },
            )
        },
    ) { pad ->
        Box(Modifier.fillMaxSize().padding(pad)) {
            val st = status
            if (st == null) {
                NoNetwork(
                    onCreate = { dialog = Dialog.CreateNetwork },
                    onJoin = { dialog = Dialog.Join },
                )
            } else {
                NetworkBody(
                    status = st,
                    pendingJoins = pendingJoins,
                    needsConsent = needsConsent,
                    onEnableRouting = onEnableRouting,
                    onToggleOnline = { online -> runAction { EngineHolder.setOnline(online) } },
                    onMember = { dialog = Dialog.MemberDetail(it) },
                    onApprove = { pj -> runAction { EngineHolder.approveJoin(pj.nodeId) } },
                    onDeny = { pj -> runAction { EngineHolder.denyJoin(pj.nodeId) } },
                )
            }

            // While *we* are joining, the engine emits the SAS before the other side
            // approves — but the network isn't active yet (status is null), so show
            // it as a top-level overlay rather than inside the (absent) member list.
            joinSas?.let { sas ->
                JoinSasDialog(sas = sas, onDismiss = { EngineHolder.clearJoinSas() })
            }
        }
    }

    // Render whichever dialog is active.
    AppDialogs(
        dialog = dialog,
        status = status,
        onDismiss = { dialog = null },
        onScanTicket = onScanTicket,
        run = runAction,
        snackbar = snackbar,
    )
}

@Composable
private fun NoNetwork(onCreate: () -> Unit, onJoin: () -> Unit) {
    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("Welcome to Nullgate", style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.size(8.dp))
        Text(
            "Connect this device to your own private LAN over the internet — no server, " +
                "no accounts. Create a new network, or join one with a ticket.",
            style = MaterialTheme.typography.bodyMedium,
        )
        Spacer(Modifier.size(24.dp))
        OutlinedButton(onClick = onCreate, modifier = Modifier.fillMaxWidth()) {
            Text("Create a network")
        }
        Spacer(Modifier.size(12.dp))
        OutlinedButton(onClick = onJoin, modifier = Modifier.fillMaxWidth()) {
            Text("Join with a ticket")
        }
    }
}

@Composable
private fun NetworkBody(
    status: NetworkStatus,
    pendingJoins: List<EngineHolder.PendingJoin>,
    needsConsent: Boolean,
    onEnableRouting: () -> Unit,
    onToggleOnline: (Boolean) -> Unit,
    onMember: (MemberView) -> Unit,
    onApprove: (EngineHolder.PendingJoin) -> Unit,
    onDeny: (EngineHolder.PendingJoin) -> Unit,
) {
    LazyColumn(Modifier.fillMaxSize().padding(horizontal = 12.dp)) {
        item {
            HeaderCard(status, needsConsent, onEnableRouting, onToggleOnline)
        }
        // Only admins act on join requests; Peers never see the approval cards
        // (mirrors the desktop GUI, which hides the join-requests area for Peers).
        if (status.selfRole != "peer") {
            items(pendingJoins, key = { it.nodeId }) { pj ->
                JoinRequestCard(pj, onApprove = { onApprove(pj) }, onDeny = { onDeny(pj) })
            }
        }
        item {
            Text(
                "Members (${status.members.size})",
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.padding(12.dp),
            )
        }
        items(status.members, key = { it.nodeId }) { m ->
            MemberRow(m, onClick = { onMember(m) })
            HorizontalDivider()
        }
    }
}

@Composable
private fun HeaderCard(
    status: NetworkStatus,
    needsConsent: Boolean,
    onEnableRouting: () -> Unit,
    onToggleOnline: (Boolean) -> Unit,
) {
    Card(Modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Column(Modifier.padding(16.dp)) {
            Text(status.name, style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.size(4.dp))
            Text(
                "You: ${status.selfIp ?: "—"} · ${status.selfRole}" +
                    if (status.frozen) " · frozen" else "",
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = FontFamily.Monospace,
            )
            status.homeRelay?.let {
                Text("Relay: $it", style = MaterialTheme.typography.bodySmall)
            }
            Spacer(Modifier.size(12.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Connected", Modifier.width(110.dp))
                Switch(checked = status.online, onCheckedChange = onToggleOnline)
                Spacer(Modifier.width(12.dp))
                Text(
                    if (status.routing) "Routing ✓"
                    else if (status.online) "Routing off" else "Offline",
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            if (status.online && !status.routing) {
                Spacer(Modifier.size(8.dp))
                OutlinedButton(onClick = onEnableRouting) {
                    Text(if (needsConsent) "Enable routing (grant VPN)" else "Enable routing")
                }
            }
        }
    }
}

@Composable
private fun JoinSasDialog(sas: List<String>, onDismiss: () -> Unit) {
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Verify these emojis") },
        text = {
            Column {
                Text(sas.joinToString("  "), style = MaterialTheme.typography.headlineSmall)
                Spacer(Modifier.size(8.dp))
                Text(
                    "You're joining. Confirm the approving device shows this same sequence " +
                        "before they accept you.",
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        },
        confirmButton = { androidx.compose.material3.TextButton(onClick = onDismiss) { Text("Got it") } },
    )
}

@Composable
private fun JoinRequestCard(
    pj: EngineHolder.PendingJoin,
    onApprove: () -> Unit,
    onDeny: () -> Unit,
) {
    Card(Modifier.fillMaxWidth().padding(vertical = 6.dp)) {
        Column(Modifier.padding(16.dp)) {
            Text("Join request: ${pj.hostname}", style = MaterialTheme.typography.titleSmall)
            Text(
                pj.nodeId.take(16) + "…",
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
            )
            Spacer(Modifier.size(8.dp))
            Text(pj.sas.joinToString("  "), style = MaterialTheme.typography.headlineSmall)
            Spacer(Modifier.size(8.dp))
            Text(
                "Approve only if these emojis match the joining device's screen.",
                style = MaterialTheme.typography.bodySmall,
            )
            Spacer(Modifier.size(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedButton(onClick = onApprove) { Text("Approve") }
                OutlinedButton(onClick = onDeny) { Text("Deny") }
            }
        }
    }
}

@Composable
private fun MemberRow(m: MemberView, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier.size(10.dp).clip(CircleShape)
                .then(Modifier),
        ) {
            Surface(
                color = if (m.online) Color(0xFF2E9E44) else Color(0xFF9AA0A6),
                shape = CircleShape,
                modifier = Modifier.size(10.dp),
            ) {}
        }
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(
                buildString {
                    append(m.label ?: m.hostname ?: m.nodeId.take(10))
                    if (m.isSelf) append(" (you)")
                },
                style = MaterialTheme.typography.bodyLarge,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                listOfNotNull(
                    m.virtualIp,
                    m.role.takeIf { it.isNotEmpty() },
                    m.location,
                    if (m.accessDisabled) "remote access off" else null,
                ).joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        m.direct?.let {
            Text(if (it) "direct" else "relay", style = MaterialTheme.typography.labelSmall)
        }
    }
}

/**
 * Overflow menu of network-wide actions, gated by the viewer's role so Peers and
 * Controllers only see what they can actually do — mirroring the desktop GUI
 * (`ipn-gui/src/main.rs`). Peer: view-only tier (Activity log, Leave). Controller:
 * also Peer ticket, Settings, Rename. Originator: everything. The engine enforces
 * the same rules server-side; this just avoids offering actions it would reject.
 *
 * With no network yet (`status == null`) only the device-level entries show —
 * relay servers, which may be exactly what this device needs configured *before*
 * it can reach the network it is about to join.
 */
@Composable
private fun NetworkMenu(
    expanded: Boolean,
    status: NetworkStatus?,
    onDismiss: () -> Unit,
    onAction: (Dialog) -> Unit,
) {
    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        if (status == null) {
            DropdownMenuItem(
                text = { Text("Relay servers") },
                onClick = { onAction(Dialog.Relays) },
            )
            return@DropdownMenu
        }

        val isPeer = status.selfRole == "peer"
        val isOriginator = status.isOriginator
        // Peer-level tickets & self-device settings: Controllers and the originator only.
        if (!isPeer) {
            DropdownMenuItem(text = { Text("Join ticket") }, onClick = { onAction(Dialog.PeerTicket) })
        }
        if (isOriginator) {
            DropdownMenuItem(
                text = { Text("Controller ticket") },
                onClick = { onAction(Dialog.ControllerTicket) },
            )
        }
        DropdownMenuItem(text = { Text("Activity log") }, onClick = { onAction(Dialog.AuditLog) })
        if (!isPeer) {
            DropdownMenuItem(text = { Text("Settings") }, onClick = { onAction(Dialog.Settings) })
        }
        // Device-level, so every tier gets it (a Peer runs its own relay config too).
        DropdownMenuItem(text = { Text("Relay servers") }, onClick = { onAction(Dialog.Relays) })
        // Network administration: rename is Controller+; the rest are originator-only.
        // The whole block is Controller+, so its leading divider rides on `!isPeer`.
        if (!isPeer) {
            HorizontalDivider()
            DropdownMenuItem(text = { Text("Rename network") }, onClick = { onAction(Dialog.Rename) })
            if (isOriginator) {
                DropdownMenuItem(
                    text = { Text(if (status.frozen) "Unfreeze network" else "Freeze network") },
                    onClick = { onAction(Dialog.ToggleFreeze) },
                )
                DropdownMenuItem(
                    text = { Text("Originator key") },
                    onClick = { onAction(Dialog.OriginatorKey) },
                )
                DropdownMenuItem(text = { Text("Rotate network") }, onClick = { onAction(Dialog.Rotate) })
                DropdownMenuItem(text = { Text("Delete network") }, onClick = { onAction(Dialog.Delete) })
            }
        }
        HorizontalDivider()
        // Restoring originator control is a recovery action for anyone holding the master
        // key who isn't the originator here — Peers included, mirroring the desktop GUI.
        if (!isOriginator) {
            DropdownMenuItem(
                text = { Text("Restore originator access") },
                onClick = { onAction(Dialog.RestoreOriginator) },
            )
        }
        DropdownMenuItem(text = { Text("Leave network") }, onClick = { onAction(Dialog.Leave) })
    }
}
