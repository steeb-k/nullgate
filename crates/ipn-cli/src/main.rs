//! Headless IPC client for `ipn-daemon` — scripting + testing without the GUI.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use ipn_ipc::transport::oneshot_request;
use ipn_ipc::{IpcRequest, IpcResponse};

#[derive(Parser)]
#[command(name = "ipn-cli", about = "Control the IPN daemon", version)]
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
    /// Join a network from a ticket.
    Join { ticket: String },
    /// Print the join ticket for the current network.
    Ticket,
    /// Approve a pending join request (by node id).
    Approve { node_id: String },
    /// Deny a pending join request.
    Deny { node_id: String },
    /// Remove a member (originator only).
    Remove { node_id: String },
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
        Cmd::Status => IpcRequest::GetStatus,
        Cmd::Create { name } => IpcRequest::CreateNetwork { name },
        Cmd::Join { ticket } => IpcRequest::Join { ticket },
        Cmd::Ticket => IpcRequest::GetTicket,
        Cmd::Approve { node_id } => IpcRequest::ApproveJoin { node_id },
        Cmd::Deny { node_id } => IpcRequest::DenyJoin { node_id },
        Cmd::Remove { node_id } => IpcRequest::RemoveMember { node_id },
        Cmd::Freeze { on } => IpcRequest::SetFrozen { frozen: on },
        Cmd::Delete => IpcRequest::DeleteNetwork,
        Cmd::Rotate => IpcRequest::RotateNetwork,
        Cmd::Leave => IpcRequest::LeaveNetwork,
        Cmd::Connect => IpcRequest::Connect,
        Cmd::Disconnect => IpcRequest::Disconnect,
        Cmd::Rename { name } => IpcRequest::SetNetworkName { name },
        Cmd::Nickname { node_id, name } => IpcRequest::SetNickname { node_id, name },
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
                "self: {}  ip: {}  originator: {}  routing: {}",
                &s.self_node_id[..16.min(s.self_node_id.len())],
                s.self_ip.unwrap_or_else(|| "-".into()),
                s.is_originator,
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
                println!(
                    "  [{}] {} {} {}{}",
                    if m.online { "online " } else { "offline" },
                    m.virtual_ip.unwrap_or_else(|| "-".into()),
                    name,
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
        IpcResponse::Hello {
            version,
            app_version,
        } => println!("daemon ipc protocol v{version} (app v{app_version})"),
        IpcResponse::Ok => println!("ok"),
        IpcResponse::Err(e) => bail!("daemon error: {e}"),
    }
    Ok(())
}
