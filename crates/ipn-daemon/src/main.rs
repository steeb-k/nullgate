//! The IPN daemon: runs the engine (iroh node + roster + mesh + presence + TUN)
//! and serves the unprivileged GUI over a local IPC socket. This is the only
//! component that needs privilege (to create the TUN); the GUI never does.
//!
//! Modes:
//!   * `run` (default) — foreground; Ctrl-C stops it. Used directly, by systemd,
//!     or by a setcap'd binary on Linux.
//!   * Windows service — `install`/`uninstall`/`start`/`stop`, and the internal
//!     `service` SCM entry point so it auto-starts as LocalSystem (no elevation
//!     for the GUI). See [`service`].

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ipn_core::Engine;
use ipn_ipc::transport::{self, read_frame, write_frame};
use ipn_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};
use tokio::io::AsyncWriteExt;

#[cfg(windows)]
mod service;

#[derive(Parser)]
#[command(name = "ipn-daemon", about = "Privileged IPN daemon (owns TUN + iroh node)")]
struct Cli {
    /// Override the data directory (node key, network config, docs).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Override the IPC socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground (default).
    Run,
    /// Install the Windows service (auto-start as LocalSystem).
    #[cfg(windows)]
    Install,
    /// Remove the Windows service.
    #[cfg(windows)]
    Uninstall,
    /// Start the installed Windows service.
    #[cfg(windows)]
    Start,
    /// Stop the installed Windows service.
    #[cfg(windows)]
    Stop,
    /// Internal: SCM entry point (used by the service manager).
    #[cfg(windows)]
    #[command(hide = true)]
    Service,
}

pub(crate) fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("IPN_DATA_DIR") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("io.github", "steeb_k", "ipn")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("ipn"))
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir.clone().unwrap_or_else(default_data_dir)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn".into()),
        )
        .try_init();
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    #[cfg(windows)]
    match cli.cmd {
        Some(Cmd::Service) => {
            // Blocks in the SCM dispatcher; builds its own runtime internally.
            service::run_as_service().map_err(|e| anyhow::anyhow!("service: {e}"))?;
            return Ok(());
        }
        Some(Cmd::Install) => {
            return service::manage("install").map_err(|e| anyhow::anyhow!("{e}"))
        }
        Some(Cmd::Uninstall) => {
            return service::manage("uninstall").map_err(|e| anyhow::anyhow!("{e}"))
        }
        Some(Cmd::Start) => return service::manage("start").map_err(|e| anyhow::anyhow!("{e}")),
        Some(Cmd::Stop) => return service::manage("stop").map_err(|e| anyhow::anyhow!("{e}")),
        _ => {}
    }

    // Foreground run.
    let data_dir = data_dir(&cli);
    let socket = cli.socket.clone().unwrap_or_else(ipn_ipc::default_socket);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve(data_dir, socket, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl-c received");
    }))
}

/// Run the engine + IPC server until `shutdown` resolves.
pub(crate) async fn serve<F>(data_dir: PathBuf, socket: PathBuf, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()>,
{
    tracing::info!("starting engine (data dir: {})", data_dir.display());
    let engine = Engine::start(&data_dir).await?;
    tracing::info!("node id: {}", engine.self_node_id_hex());

    let listener = transport::bind(&socket)?;
    tracing::info!("listening on {}", socket.display());

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
            res = transport::accept(&listener) => match res {
                Ok(stream) => {
                    let engine = engine.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(engine, stream).await {
                            tracing::debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
    Ok(())
}

async fn handle_conn(engine: Engine, stream: transport::Stream) -> Result<()> {
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    while let Some(frame) = read_frame(&mut reader).await? {
        let Message::Request(req) = frame.body else {
            continue;
        };

        if matches!(req, IpcRequest::Subscribe) {
            let st = engine.status().await.ok();
            send_event(&writer, IpcEvent::Status(st)).await?;
            let mut ev = engine.subscribe();
            while let Ok(e) = ev.recv().await {
                let ipc = map_event(&engine, e).await;
                if send_event(&writer, ipc).await.is_err() {
                    break;
                }
            }
            return Ok(());
        }

        let resp = handle_request(&engine, req).await;
        let mut w = writer.lock().await;
        write_frame(
            &mut *w,
            &Frame {
                id: frame.id,
                body: Message::Response(resp),
            },
        )
        .await?;
        w.flush().await?;
    }
    Ok(())
}

async fn send_event(
    writer: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<transport::Stream>>>,
    ev: IpcEvent,
) -> Result<()> {
    let mut w = writer.lock().await;
    write_frame(
        &mut *w,
        &Frame {
            id: 0,
            body: Message::Event(ev),
        },
    )
    .await?;
    Ok(())
}

async fn map_event(engine: &Engine, e: ipn_core::EngineEvent) -> IpcEvent {
    use ipn_core::EngineEvent as E;
    match e {
        E::Changed => IpcEvent::Status(engine.status().await.ok()),
        E::JoinSas { sas } => IpcEvent::JoinSas { sas },
        E::JoinRequest {
            node_id,
            hostname,
            sas,
        } => IpcEvent::JoinRequest {
            node_id,
            hostname,
            sas,
        },
    }
}

async fn handle_request(engine: &Engine, req: IpcRequest) -> IpcResponse {
    use std::net::Ipv4Addr;
    let to_err = |e: anyhow::Error| IpcResponse::Err(format!("{e:#}"));
    match req {
        IpcRequest::GetStatus => IpcResponse::Status(engine.status().await.ok()),
        IpcRequest::CreateNetwork { name } => {
            match engine.create_network(name, Ipv4Addr::new(10, 99, 0, 0)).await {
                Ok(t) => IpcResponse::Ticket(t),
                Err(e) => to_err(e),
            }
        }
        IpcRequest::Join { ticket } => match engine.join_network(&ticket).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::ApproveJoin { node_id } => match engine.approve_join(&node_id).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::DenyJoin { node_id } => match engine.deny_join(&node_id).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::RemoveMember { node_id } => match engine.remove_member(&node_id).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::SetFrozen { frozen } => match engine.set_frozen(frozen).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::DeleteNetwork => match engine.delete_network().await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::RotateNetwork => match engine.rotate_network().await {
            Ok(t) => IpcResponse::Ticket(t),
            Err(e) => to_err(e),
        },
        IpcRequest::LeaveNetwork => match engine.leave_network().await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::Connect => match engine.set_online(true).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::Disconnect => match engine.set_online(false).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::GetTicket => match engine.ticket().await {
            Ok(t) => IpcResponse::Ticket(t),
            Err(e) => to_err(e),
        },
        IpcRequest::Subscribe => IpcResponse::Ok,
    }
}
