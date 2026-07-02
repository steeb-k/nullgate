//! Headless IPC client for `ipn-daemon` — scripting + testing without the GUI.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use ipn_ipc::transport::{self, oneshot_request, read_frame, write_frame};
use ipn_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};

#[derive(Parser)]
#[command(name = "nullgate-cli", about = "Control the Nullgate daemon", version)]
struct Cli {
    #[arg(long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the current network + members.
    Status,
    /// Create a new network (this device becomes originator); prints the ticket.
    Create { name: String },
    /// Join a network from a ticket. Prints the verification words to read aloud
    /// to the approving member, then blocks until they approve or deny.
    Join { ticket: String },
    /// Stream events: print incoming join requests (with verification words to
    /// compare) and status changes. Run this to approve joins headlessly.
    Watch,
    /// Print a join ticket. Peer-level by default; `--controller` for a single-use
    /// Controller ticket (originator only); `--single-use` toggles Peer single-use.
    Ticket {
        #[arg(long)]
        controller: bool,
        #[arg(long)]
        single_use: Option<bool>,
    },
    /// Approve a pending join request (by node id).
    Approve { node_id: String },
    /// Deny a pending join request.
    Deny { node_id: String },
    /// Remove a member (originator, or a Controller removing a Peer).
    Remove { node_id: String },
    /// Promote/demote a member (originator only).
    Role {
        node_id: String,
        /// `controller` or `peer`.
        tier: String,
    },
    /// Show the administration activity log.
    Log,
    /// Disable inbound remote access on this device (one-way block).
    Block,
    /// Re-enable inbound remote access on this device.
    Unblock,
    /// Hide this device from the member list (implies the inbound block).
    Hide,
    /// Stop hiding this device from the member list.
    Unhide,
    /// Freeze or unfreeze membership (originator only).
    Freeze {
        #[arg(value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// Originator-only: dissolve the network (boots all members), then leave.
    Delete,
    /// Originator-only: rotate the secret (mass-revoke); prints the new ticket.
    Rotate,
    /// Leave the network on this device only.
    Leave,
    /// Connect to the saved network (go online).
    Connect,
    /// Disconnect but keep the network saved (go offline).
    Disconnect,
    /// Rename the network (shared across all members).
    Rename { name: String },
    /// Set a local friendly nickname for another member (omit the name to clear it).
    Nickname { node_id: String, name: Option<String> },
    /// Set a local free-text note for another member (omit the text to clear it).
    Note { node_id: String, note: Option<String> },
    /// Export the originator recovery code (originator only).
    ExportKey,
    /// Import an originator recovery code to gain originator powers.
    ImportKey { code: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(ipn_ipc::default_socket);

    let req = match cli.cmd {
        // These drive server-pushed events (the emoji SAS shown as words), so they
        // can't use the one-shot request/response path.
        Cmd::Join { ticket } => return cmd_join(&socket, ticket).await,
        Cmd::Watch => return cmd_watch(&socket).await,
        Cmd::Status => IpcRequest::GetStatus,
        Cmd::Create { name } => IpcRequest::CreateNetwork { name },
        Cmd::Ticket {
            controller,
            single_use,
        } => match (single_use, controller) {
            (Some(on), _) => IpcRequest::SetPeerTicketSingleUse { on },
            (None, true) => IpcRequest::GetControllerTicket,
            (None, false) => IpcRequest::GetTicket,
        },
        Cmd::Approve { node_id } => IpcRequest::ApproveJoin { node_id },
        Cmd::Deny { node_id } => IpcRequest::DenyJoin { node_id },
        Cmd::Remove { node_id } => IpcRequest::RemoveMember { node_id },
        Cmd::Role { node_id, tier } => IpcRequest::SetMemberRole {
            node_id,
            controller: matches!(tier.to_lowercase().as_str(), "controller" | "c"),
        },
        Cmd::Log => IpcRequest::GetAuditLog,
        Cmd::Block => IpcRequest::SetRemoteAccessDisabled { disabled: true },
        Cmd::Unblock => IpcRequest::SetRemoteAccessDisabled { disabled: false },
        Cmd::Hide => IpcRequest::SetHidden { hidden: true },
        Cmd::Unhide => IpcRequest::SetHidden { hidden: false },
        Cmd::Freeze { on } => IpcRequest::SetFrozen { frozen: on },
        Cmd::Delete => IpcRequest::DeleteNetwork,
        Cmd::Rotate => IpcRequest::RotateNetwork,
        Cmd::Leave => IpcRequest::LeaveNetwork,
        Cmd::Connect => IpcRequest::Connect,
        Cmd::Disconnect => IpcRequest::Disconnect,
        Cmd::Rename { name } => IpcRequest::SetNetworkName { name },
        Cmd::Nickname { node_id, name } => IpcRequest::SetNickname { node_id, name },
        Cmd::Note { node_id, note } => IpcRequest::SetNote { node_id, note },
        Cmd::ExportKey => IpcRequest::ExportOriginatorKey,
        Cmd::ImportKey { code } => IpcRequest::ImportOriginatorKey { code },
    };

    let resp = oneshot_request(&socket, req)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach daemon at {}: {e}", socket.display()))?;

    match resp {
        IpcResponse::Status(None) => println!("no network on this device"),
        IpcResponse::Status(Some(s)) => {
            println!("network: {}  subnet: {}  frozen: {}", s.name, s.subnet, s.frozen);
            println!(
                "self: {}  ip: {}  role: {}  routing: {}",
                &s.self_node_id[..16.min(s.self_node_id.len())],
                s.self_ip.unwrap_or_else(|| "-".into()),
                s.self_role,
                s.routing
            );
            for m in s.members {
                if m.is_self {
                    continue;
                }
                let host = m
                    .hostname
                    .clone()
                    .unwrap_or_else(|| m.node_id[..16.min(m.node_id.len())].into());
                let name = match m.label {
                    Some(l) => format!("{l} ({host})"),
                    None => host,
                };
                let flag = if m.hidden {
                    " [hidden]"
                } else if m.access_disabled {
                    " [access disabled]"
                } else {
                    ""
                };
                println!(
                    "  [{}] {} {} ({}){} {}{}",
                    if m.online { "online " } else { "offline" },
                    m.virtual_ip.unwrap_or_else(|| "-".into()),
                    name,
                    m.role,
                    flag,
                    m.observed_addr.unwrap_or_default(),
                    match m.direct {
                        Some(true) => " (direct)",
                        Some(false) => " (relay)",
                        None => "",
                    }
                );
            }
        }
        IpcResponse::Ticket(t) => println!("{t}"),
        IpcResponse::Recovery(code) => println!("{code}"),
        IpcResponse::AuditLog(entries) => {
            if entries.is_empty() {
                println!("(no activity in the last 30 days)");
            }
            for e in entries {
                let who = e.actor_name.unwrap_or(e.actor_node_id);
                println!("  {}  {}  {}", e.ts, who, e.action);
            }
        }
        IpcResponse::Hello {
            version,
            app_version,
        } => println!("daemon ipc protocol v{version} (app v{app_version})"),
        IpcResponse::Ok => println!("ok"),
        IpcResponse::Err(e) => bail!("daemon error: {e}"),
    }
    Ok(())
}

/// Print a SAS as a numbered word list. The emojis are meaningless over a terminal,
/// so both sides compare these words instead (they're derived from the same code).
fn print_sas(prompt: &str, sas: &[String]) {
    println!("\n{prompt}:");
    for (i, word) in ipn_ipc::sas_words(sas).iter().enumerate() {
        println!("  {}. {word}", i + 1);
    }
    println!();
}

/// Open an event subscription on its own connection. The daemon takes the
/// connection over for pushed events, so it can't also serve requests.
async fn subscribe(socket: &Path) -> Result<transport::Stream> {
    let stream = transport::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach daemon at {}: {e}", socket.display()))?;
    Ok(stream)
}

/// Join, showing our verification words while we wait. We subscribe first (so we
/// don't miss the SAS the daemon computes mid-handshake) and fire the blocking
/// `Join` on a second connection.
async fn cmd_join(socket: &Path, ticket: String) -> Result<()> {
    let stream = subscribe(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: Message::Request(IpcRequest::Subscribe),
        },
    )
    .await?;

    println!("Requesting to join. Waiting for a member to approve…");

    let join_fut = oneshot_request(socket, IpcRequest::Join { ticket });
    tokio::pin!(join_fut);
    let mut sub_open = true;

    loop {
        tokio::select! {
            resp = &mut join_fut => {
                return match resp.map_err(|e| anyhow::anyhow!("can't reach daemon: {e}"))? {
                    IpcResponse::Ok => { println!("Approved — this device is now in the network."); Ok(()) }
                    IpcResponse::Err(e) => bail!("join failed: {e}"),
                    other => bail!("unexpected daemon response: {other:?}"),
                };
            }
            frame = read_frame(&mut reader), if sub_open => {
                match frame? {
                    Some(f) => {
                        if let Message::Event(IpcEvent::JoinSas { sas }) = f.body {
                            print_sas("Read these words to the person approving you — they must match on their screen", &sas);
                        }
                    }
                    None => sub_open = false,
                }
            }
        }
    }
}

/// Stream events: incoming join requests (with words to compare) and status
/// changes. Used to approve joins on a headless box.
async fn cmd_watch(socket: &Path) -> Result<()> {
    let stream = subscribe(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: Message::Request(IpcRequest::Subscribe),
        },
    )
    .await?;

    println!("Watching for join requests. Press Ctrl-C to stop.");
    while let Some(frame) = read_frame(&mut reader).await? {
        let Message::Event(ev) = frame.body else { continue };
        match ev {
            IpcEvent::JoinRequest { node_id, hostname, sas } => {
                let short = &node_id[..16.min(node_id.len())];
                println!("\nJoin request from \"{hostname}\" ({short}…)");
                print_sas("Approve only if these words match the joining device's screen", &sas);
                println!("  approve:  nullgate-cli approve {node_id}");
                println!("  deny:     nullgate-cli deny {node_id}");
            }
            IpcEvent::JoinSas { sas } => {
                print_sas("This device is joining — read these words to the approver", &sas);
            }
            IpcEvent::Status(_) => {}
        }
    }
    Ok(())
}
