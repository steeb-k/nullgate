//! IPC contract between the unprivileged IPN **GUI** and the privileged
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
pub use ipn_core::{MemberView, NetworkStatus};

/// IPC wire-protocol version between the GUI/CLI and the daemon. Bump on any
/// incompatible change to these request/response/event types.
pub const PROTO_VERSION: u32 = 1;

/// Where the GUI and daemon rendezvous. On Windows this path is only hashed into
/// a named-pipe name; on Unix it's the actual socket path (fixed, not `$TMPDIR`,
/// so a root daemon and a user GUI agree).
pub fn default_socket() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
        base.join("ipn").join("ipn.sock")
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/tmp/ipn.sock")
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
    GetTicket,
    /// Rename the network (shared across members via the signed roster).
    SetNetworkName { name: String },
    /// Set (or clear, with `None`) this client's **local** friendly nickname for
    /// another member (by NodeId hex). Never broadcast; the hostname is the shared
    /// identifier.
    SetNickname { node_id: String, name: Option<String> },
    /// Export the originator master key as a recovery code (originator only).
    ExportOriginatorKey,
    /// Import an originator recovery code to gain originator powers on this network.
    ImportOriginatorKey { code: String },
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
