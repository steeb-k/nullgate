package io.github.steeb_k.nullgate.ui

import androidx.compose.foundation.Image
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
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.google.zxing.BarcodeFormat
import com.journeyapps.barcodescanner.BarcodeEncoder
import io.github.steeb_k.nullgate.engine.EngineHolder
import kotlinx.coroutines.launch
import uniffi.ipn_mobile.MemberView
import uniffi.ipn_mobile.NetworkStatus

/** Which modal is currently open. */
sealed interface Dialog {
    data object CreateNetwork : Dialog
    data object Join : Dialog
    data object PeerTicket : Dialog
    data object ControllerTicket : Dialog
    data object AuditLog : Dialog
    data object Settings : Dialog
    /** Device-level, so it is reachable with or without a network. */
    data object Relays : Dialog
    data object Rename : Dialog
    data object ToggleFreeze : Dialog
    data object OriginatorKey : Dialog
    data object RestoreOriginator : Dialog
    data object Rotate : Dialog
    data object Delete : Dialog
    data object Leave : Dialog
    data class MemberDetail(val member: MemberView) : Dialog
}

@Composable
fun AppDialogs(
    dialog: Dialog?,
    status: NetworkStatus?,
    onDismiss: () -> Unit,
    onScanTicket: ((String?) -> Unit) -> Unit,
    run: (suspend () -> Unit) -> Unit,
    snackbar: androidx.compose.material3.SnackbarHostState,
) {
    when (dialog) {
        null -> Unit
        Dialog.CreateNetwork -> CreateNetworkDialog(onDismiss)
        Dialog.Join -> JoinDialog(onDismiss, onScanTicket)
        Dialog.PeerTicket -> PeerTicketDialog(status, onDismiss)
        Dialog.ControllerTicket -> SimpleTicketDialog("Controller ticket", onDismiss) {
            EngineHolder.controllerTicket()
        }
        Dialog.AuditLog -> AuditLogDialog(onDismiss)
        Dialog.Settings -> SettingsDialog(status, onDismiss)
        Dialog.Relays -> RelaysDialog(onDismiss)
        Dialog.Rename -> TextPromptDialog(
            title = "Rename network",
            initial = status?.name.orEmpty(),
            label = "Network name",
            onDismiss = onDismiss,
        ) { run { EngineHolder.setNetworkName(it) }; onDismiss() }
        Dialog.ToggleFreeze -> ConfirmDialog(
            title = if (status?.frozen == true) "Unfreeze network?" else "Freeze network?",
            body = if (status?.frozen == true)
                "New devices will be able to join again."
            else "No new devices can join while frozen.",
            onDismiss = onDismiss,
        ) { run { EngineHolder.setFrozen(status?.frozen != true) }; onDismiss() }
        Dialog.OriginatorKey -> OriginatorKeyDialog(onDismiss, allowExport = true)
        Dialog.RestoreOriginator -> OriginatorKeyDialog(onDismiss, allowExport = false)
        Dialog.Rotate -> ConfirmDialog(
            title = "Rotate network?",
            body = "This revokes every current member and issues a fresh ticket. Members must " +
                "re-join with the new ticket.",
            onDismiss = onDismiss,
        ) { run { EngineHolder.rotateNetwork() }; onDismiss() }
        Dialog.Delete -> ConfirmDialog(
            title = "Delete network?",
            body = "This dissolves the network for everyone. This cannot be undone.",
            onDismiss = onDismiss,
        ) { run { EngineHolder.deleteNetwork() }; onDismiss() }
        Dialog.Leave -> ConfirmDialog(
            title = "Leave network?",
            body = "This device will forget the network. You can re-join later with a ticket.",
            onDismiss = onDismiss,
        ) { run { EngineHolder.leaveNetwork() }; onDismiss() }
        is Dialog.MemberDetail -> MemberDetailDialog(dialog.member, status, onDismiss, run)
    }
}

@Composable
private fun CreateNetworkDialog(onDismiss: () -> Unit) {
    var name by remember { mutableStateOf("") }
    var ticket by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboardManager.current

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (ticket == null) "Create a network" else "Network created") },
        text = {
            if (ticket == null) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Network name") },
                    singleLine = true,
                )
            } else {
                Column {
                    Text("Share this ticket with a device to let it join:")
                    Spacer(Modifier.size(8.dp))
                    QrAndText(ticket!!)
                }
            }
        },
        confirmButton = {
            if (ticket == null) {
                TextButton(enabled = name.isNotBlank() && !busy, onClick = {
                    busy = true
                    scope.launch {
                        runCatching { EngineHolder.createNetwork(name.trim()) }
                            .onSuccess { ticket = it }
                            .onFailure { onDismiss() }
                        busy = false
                    }
                }) { Text("Create") }
            } else {
                TextButton(onClick = {
                    clipboard.setText(AnnotatedString(ticket!!)); onDismiss()
                }) { Text("Copy & close") }
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

@Composable
private fun JoinDialog(onDismiss: () -> Unit, onScanTicket: ((String?) -> Unit) -> Unit) {
    var ticket by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Join a network") },
        text = {
            Column {
                OutlinedTextField(
                    value = ticket,
                    onValueChange = { ticket = it },
                    label = { Text("Ticket (ng1…)") },
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.size(8.dp))
                TextButton(onClick = { onScanTicket { it?.let { s -> ticket = s } } }) {
                    Text("Scan QR")
                }
            }
        },
        confirmButton = {
            TextButton(enabled = ticket.isNotBlank(), onClick = {
                val t = ticket.trim()
                scope.launch { runCatching { EngineHolder.joinNetwork(t) } }
                onDismiss()
            }) { Text("Join") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun PeerTicketDialog(status: NetworkStatus?, onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var ticket by remember { mutableStateOf<String?>(null) }
    var singleUse by remember { mutableStateOf(status?.peerTicketSingleUse ?: false) }
    LaunchedEffect(Unit) { ticket = runCatching { EngineHolder.ticket() }.getOrNull() }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Join ticket") },
        text = {
            Column {
                val t = ticket
                if (t == null) Text("Loading…") else QrAndText(t)
                // The single-use switch is an originator control (matches the desktop
                // admin section); Controllers can still view/share the ticket itself.
                if (status?.isOriginator == true) {
                    Spacer(Modifier.size(12.dp))
                    Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                        Switch(checked = singleUse, onCheckedChange = {
                            singleUse = it
                            scope.launch {
                                runCatching { EngineHolder.setPeerTicketSingleUse(it) }
                                ticket = runCatching { EngineHolder.ticket() }.getOrNull()
                            }
                        })
                        Spacer(Modifier.size(8.dp))
                        Text("Single-use ticket")
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

@Composable
private fun SimpleTicketDialog(
    title: String,
    onDismiss: () -> Unit,
    load: suspend () -> String,
) {
    var ticket by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { ticket = runCatching { load() }.getOrElse { "Error: ${it.message}" } }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { ticket?.let { QrAndText(it) } ?: Text("Loading…") },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

@Composable
private fun AuditLogDialog(onDismiss: () -> Unit) {
    var entries by remember { mutableStateOf<List<String>?>(null) }
    LaunchedEffect(Unit) {
        entries = runCatching {
            EngineHolder.auditLog().map { "${it.action} — ${it.actorName ?: it.actorNodeId.take(10)}" }
        }.getOrElse { listOf("Error: ${it.message}") }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Activity log") },
        text = {
            val e = entries
            if (e == null) Text("Loading…")
            else Column(Modifier.heightIn(max = 360.dp).verticalScroll(rememberScrollState())) {
                if (e.isEmpty()) Text("No activity yet.")
                e.forEach { Text(it, Modifier.padding(vertical = 4.dp), style = MaterialTheme.typography.bodySmall) }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

@Composable
private fun SettingsDialog(status: NetworkStatus?, onDismiss: () -> Unit) {
    val self = status?.members?.firstOrNull { it.isSelf }
    var hidden by remember { mutableStateOf(self?.hidden ?: false) }
    var accessOff by remember { mutableStateOf(self?.accessDisabled ?: false) }
    val scope = rememberCoroutineScope()

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Settings") },
        text = {
            Column {
                ToggleRow("Disable remote access", "Block others from reaching this device (you can still reach them).", accessOff) {
                    accessOff = it
                    scope.launch { runCatching { EngineHolder.setRemoteAccessDisabled(it) } }
                }
                Spacer(Modifier.size(8.dp))
                ToggleRow("Hide from member list", "Ask other members not to show this device (implies the block).", hidden) {
                    hidden = it
                    scope.launch { runCatching { EngineHolder.setHidden(it) } }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

/**
 * Master-key export/import. With [allowExport] (originator only) this backs up the key
 * *and* offers import; without it, it's the "Restore originator access" flow any member
 * can use to take over originator control by pasting a key exported elsewhere. Mirrors the
 * desktop Administration flyout, which shows backup-vs-restore by originator status.
 */
@Composable
private fun OriginatorKeyDialog(onDismiss: () -> Unit, allowExport: Boolean) {
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboardManager.current
    var exported by remember { mutableStateOf<String?>(null) }
    var importCode by remember { mutableStateOf("") }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (allowExport) "Originator key" else "Restore originator access") },
        text = {
            Column {
                Text(
                    if (allowExport)
                        "Export the master recovery key, or import one on a new device."
                    else
                        "Paste the master recovery key exported from the originator device to " +
                            "take over originator control on this device.",
                )
                Spacer(Modifier.size(8.dp))
                if (allowExport) {
                    exported?.let {
                        Text(it, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                        TextButton(onClick = { clipboard.setText(AnnotatedString(it)) }) { Text("Copy") }
                    }
                    TextButton(onClick = {
                        scope.launch { exported = runCatching { EngineHolder.exportOriginatorKey() }.getOrNull() }
                    }) { Text("Export key") }
                    Spacer(Modifier.size(12.dp))
                }
                OutlinedTextField(
                    value = importCode,
                    onValueChange = { importCode = it },
                    label = { Text("Import (ngkey1…)") },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(enabled = importCode.isNotBlank(), onClick = {
                val code = importCode.trim()
                scope.launch { runCatching { EngineHolder.importOriginatorKey(code) } }
                onDismiss()
            }) { Text("Import") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

@Composable
private fun MemberDetailDialog(
    member: MemberView,
    status: NetworkStatus?,
    onDismiss: () -> Unit,
    run: (suspend () -> Unit) -> Unit,
) {
    var nickname by remember { mutableStateOf(member.label.orEmpty()) }
    var note by remember { mutableStateOf(member.note.orEmpty()) }
    val isOriginator = status?.isOriginator == true
    // Originator may remove anyone; a Controller may remove Peers only (mirrors the
    // roster rule + desktop GUI). Never offered on your own row.
    val canRemove = !member.isSelf && (isOriginator ||
        (status?.selfRole == "controller" && member.role == "peer"))

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(member.hostname ?: member.nodeId.take(12)) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                DetailLine("Node", member.nodeId)
                member.virtualIp?.let { DetailLine("Virtual IP", it) }
                member.localIp?.let { DetailLine("Local IP", it) }
                member.publicIp?.let { DetailLine("Public IP", it) }
                member.location?.let { DetailLine("Location", it) }
                member.observedAddr?.let { DetailLine("Observed", it) }
                DetailLine("Role", member.role)
                Spacer(Modifier.size(8.dp))
                OutlinedTextField(
                    value = nickname,
                    onValueChange = { nickname = it },
                    label = { Text("Nickname (local only)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.size(8.dp))
                OutlinedTextField(
                    value = note,
                    onValueChange = { note = it },
                    label = { Text("Note (local only)") },
                    modifier = Modifier.fillMaxWidth(),
                )
                if (!member.isSelf && isOriginator) {
                    Spacer(Modifier.size(8.dp))
                    val controller = member.role == "controller"
                    TextButton(onClick = {
                        run { EngineHolder.setMemberRole(member.nodeId, !controller) }; onDismiss()
                    }) { Text(if (controller) "Demote to Peer" else "Promote to Controller") }
                }
                if (canRemove) {
                    TextButton(onClick = {
                        run { EngineHolder.removeMember(member.nodeId) }; onDismiss()
                    }) { Text("Remove from network") }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                run {
                    EngineHolder.setNickname(member.nodeId, nickname.ifBlank { null })
                    EngineHolder.setNote(member.nodeId, note.ifBlank { null })
                }
                onDismiss()
            }) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

// --- small reusable pieces -------------------------------------------------

@Composable
private fun ConfirmDialog(
    title: String,
    body: String,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(body) },
        confirmButton = { TextButton(onClick = onConfirm) { Text("Confirm") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun TextPromptDialog(
    title: String,
    initial: String,
    label: String,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    var value by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = value,
                onValueChange = { value = it },
                label = { Text(label) },
                singleLine = true,
            )
        },
        confirmButton = {
            TextButton(enabled = value.isNotBlank(), onClick = { onConfirm(value.trim()) }) {
                Text("Save")
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ToggleRow(title: String, subtitle: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(subtitle, style = MaterialTheme.typography.bodySmall)
        }
        Switch(checked = checked, onCheckedChange = onChange)
    }
}

@Composable
private fun DetailLine(label: String, value: String) {
    Column(Modifier.padding(vertical = 2.dp)) {
        Text(label, style = MaterialTheme.typography.labelSmall)
        Text(value, style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
    }
}

/** A copyable string with a QR code above it (tickets / recovery keys). */
@Composable
private fun QrAndText(text: String) {
    val clipboard = LocalClipboardManager.current
    val bmp = remember(text) {
        runCatching {
            BarcodeEncoder().encodeBitmap(text, BarcodeFormat.QR_CODE, 600, 600)
        }.getOrNull()
    }
    Column {
        bmp?.let {
            Image(it.asImageBitmap(), contentDescription = "QR code", modifier = Modifier.size(220.dp))
        }
        Spacer(Modifier.size(8.dp))
        Text(text, style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
        TextButton(onClick = { clipboard.setText(AnnotatedString(text)) }) { Text("Copy") }
    }
}
