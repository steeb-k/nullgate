//! IPC contract between the unprivileged Nullgate **GUI** and the privileged
//! **ipn-daemon** (which owns the TUN + iroh node). Deliberately light — the GUI
//! never needs to run the engine or create a TUN itself, so it never needs
//! elevation; the daemon (a service / setcap binary) does the privileged work.
//!
//! Framing: a `u64` correlation id + a [`Message`]. `id == 0` marks a
//! server-pushed [`IpcEvent`]; nonzero ids correlate a request with its response.

use serde::{Deserialize, Serialize};

#[cfg(feature = "transport")]
pub mod transport;

/// Display DTOs are reused straight from the engine crate (plain serde structs).
pub use ipn_core::{AuditEntry, MemberView, NetworkStatus, RelayPolicy, RelayServer, RelaySettings};

/// Render a SAS (the emoji strings carried on [`IpcEvent::JoinSas`] /
/// [`IpcEvent::JoinRequest`]) as words, for text-only clients like the CLI.
pub use ipn_core::admission::sas_words;

/// IPC wire-protocol version between the GUI/CLI and the daemon. Bump on any
/// incompatible change to these request/response/event types.
///
/// v2 (0.1.5): added the privilege-tier / access-control requests
/// (`SetMemberRole`, `GetControllerTicket`, `SetPeerTicketSingleUse`,
/// `SetRemoteAccessDisabled`, `SetHidden`, `GetAuditLog`) and the `AuditLog`
/// response. A v1 daemon can't decode these, so the version handshake must reject
/// the pairing rather than let requests silently fail.
///
/// v3 (0.1.7): added the per-member local-note request (`SetNote`) and the
/// `MemberView.note` field.
///
/// v4 (0.3.2): added the custom relay server requests (`GetRelays`, `SetRelays`)
/// and the `Relays` response.
pub const PROTO_VERSION: u32 = 4;

/// Where the GUI and daemon rendezvous. On Windows this path is only hashed into
/// a named-pipe name; on Unix it's the actual socket path (fixed, not `$TMPDIR`,
/// so a root daemon and a user GUI agree).
pub fn default_socket() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
        base.join("nullgate").join("nullgate.sock")
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/tmp/nullgate.sock")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub body: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Request(IpcRequest),
    Response(IpcResponse),
    Event(IpcEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Version handshake; the daemon replies with [`IpcResponse::Hello`].
    Hello { version: u32 },
    GetStatus,
    CreateNetwork { name: String },
    Join { ticket: String },
    ApproveJoin { node_id: String },
    DenyJoin { node_id: String },
    RemoveMember { node_id: String },
    /// Originator-only: promote/demote a member (`controller=false` ⇒ Peer).
    SetMemberRole { node_id: String, controller: bool },
    SetFrozen { frozen: bool },
    /// Originator-only: dissolve the network (boots all members), then leave.
    DeleteNetwork,
    /// Originator-only: rotate the network secret (mass-revoke); returns a new ticket.
    RotateNetwork,
    /// Leave the network on this device only.
    LeaveNetwork,
    /// Connect to the saved network (go online). Idempotent.
    Connect,
    /// Disconnect from the network but keep it saved (go offline). Idempotent.
    Disconnect,
    /// The Peer-level join ticket (Controllers and the originator).
    GetTicket,
    /// Originator-only: a fresh single-use Controller-level join ticket.
    GetControllerTicket,
    /// Originator/Controller: toggle whether Peer tickets are single-use. Mints a
    /// new code, invalidating the previous one for new joins.
    SetPeerTicketSingleUse { on: bool },
    /// Toggle this device's one-way inbound block (outbound still works).
    SetRemoteAccessDisabled { disabled: bool },
    /// Toggle hiding this device from the member list (implies the inbound block).
    SetHidden { hidden: bool },
    /// Fetch the 30-day administration activity log (visible to all members).
    GetAuditLog,
    /// Rename the network (shared across members via the signed roster).
    SetNetworkName { name: String },
    /// Set (or clear, with `None`) this client's **local** friendly nickname for
    /// another member (by NodeId hex). Never broadcast; the hostname is the shared
    /// identifier.
    SetNickname { node_id: String, name: Option<String> },
    /// Set (or clear, with `None`) this client's **local** free-text note for a
    /// member (by NodeId hex). Never broadcast.
    SetNote { node_id: String, note: Option<String> },
    /// Export the originator master key as a recovery code (originator only).
    ExportOriginatorKey,
    /// Import an originator recovery code to gain originator powers on this network.
    ImportOriginatorKey { code: String },
    /// Fetch this device's custom relay server configuration.
    GetRelays,
    /// Replace this device's custom relay server configuration. Applies to the
    /// live endpoint immediately (no daemon restart).
    SetRelays { settings: RelaySettings },
    /// Upgrade this connection to receive pushed [`IpcEvent`]s.
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    /// The daemon's IPC protocol version + its app (semver) version (reply to
    /// [`IpcRequest::Hello`]). `app_version` lets the GUI notice it has gone stale
    /// after an auto-update and relaunch itself. `default` for back-compat with an
    /// older daemon that didn't send it.
    Hello {
        version: u32,
        #[serde(default)]
        app_version: String,
    },
    /// `None` when this device isn't in a network yet.
    Status(Option<NetworkStatus>),
    Ticket(String),
    /// An originator recovery code (reply to `ExportOriginatorKey`).
    Recovery(String),
    /// The administration activity log (reply to `GetAuditLog`).
    AuditLog(Vec<AuditEntry>),
    /// This device's custom relay configuration (reply to `GetRelays`).
    Relays(RelaySettings),
    Ok,
    Err(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcEvent {
    Status(Option<NetworkStatus>),
    JoinSas {
        sas: Vec<String>,
    },
    JoinRequest {
        node_id: String,
        hostname: String,
        sas: Vec<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("cbor encode: {0}")]
    Encode(String),
    #[error("cbor decode: {0}")]
    Decode(String),
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::new();
    ciborium::into_writer(frame, &mut buf).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

pub fn decode(bytes: &[u8]) -> Result<Frame, CodecError> {
    ciborium::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}
