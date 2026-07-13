//! The Nullgate daemon: runs the engine (iroh node + roster + mesh + presence + TUN)
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

mod logging;
mod power;
#[cfg(windows)]
mod service;
mod watchdog;

#[derive(Parser)]
#[command(name = "nullgate-daemon", about = "Privileged Nullgate daemon (owns TUN + iroh node)", version)]
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
    /// Stop then start the installed Windows service (used by the app's elevated
    /// "Restart Nullgate daemon" action, so no PowerShell is involved).
    #[cfg(windows)]
    Restart,
    /// (Re)configure auto-restart recovery on the installed Windows service.
    #[cfg(windows)]
    Recover,
    /// Internal: SCM entry point (used by the service manager).
    #[cfg(windows)]
    #[command(hide = true)]
    Service,
}

/// Deliberately trigger a non-panic abort, to validate the Windows crash-capture
/// nets (VEH + reclaimed stderr) that the panic hook can't cover.
fn crash_selftest(kind: &str) -> ! {
    match kind {
        // Null dereference → access violation (0xC0000005), caught first-chance
        // by the vectored exception handler.
        "av" => unsafe {
            std::ptr::null_mut::<u8>().write_volatile(1);
            std::process::abort()
        },
        // Unbounded recursion → stack overflow. The Rust runtime prints
        // "has overflowed its stack" to the (reclaimed) stderr before aborting.
        "stackoverflow" => {
            #[allow(unconditional_recursion)]
            fn recurse(x: u64) -> u64 {
                let pad = std::hint::black_box([x; 256]);
                std::hint::black_box(pad[0]).wrapping_add(recurse(x.wrapping_add(1)))
            }
            std::hint::black_box(recurse(std::hint::black_box(1)));
            std::process::abort()
        }
        // Bare abort → 0xC0000409 fastfail. Bypasses the handlers by design; only
        // WER LocalDumps can capture this one (prints nothing to stderr).
        _ => std::process::abort(),
    }
}

pub(crate) fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NULLGATE_DATA_DIR") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("io.github", "steeb_k", "Nullgate")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("nullgate"))
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir.clone().unwrap_or_else(default_data_dir)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up file logging + the crash hook before doing anything that can panic
    // at runtime. `_log_guard` flushes the async file writer on a clean exit and
    // must live for the whole process (including while the Windows service
    // dispatcher blocks), so keep it bound in `main`.
    let dd = data_dir(&cli);
    let (_log_guard, log_dir) = logging::init(&dd);
    tracing::info!(
        "nullgate-daemon {} starting (pid {}); logs -> {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        log_dir.display()
    );

    // Opt-in self-tests for the crash → crash-log → auto-restart pipeline. Off by
    // default; handy for confirming service recovery + capture on a real install.
    if std::env::var_os("NULLGATE_PANIC_SELFTEST").is_some() {
        panic!("NULLGATE_PANIC_SELFTEST: forced panic to exercise crash logging + recovery");
    }
    // Non-panic abort classes the panic hook can't see (this is what the real
    // 0xc0000409 crash appears to be): NULLGATE_CRASH_SELFTEST=av|stackoverflow|abort.
    if let Some(kind) = std::env::var_os("NULLGATE_CRASH_SELFTEST") {
        crash_selftest(&kind.to_string_lossy());
    }

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
        Some(Cmd::Restart) => {
            return service::manage("restart").map_err(|e| anyhow::anyhow!("{e}"))
        }
        Some(Cmd::Recover) => {
            return service::manage("recover").map_err(|e| anyhow::anyhow!("{e}"))
        }
        _ => {}
    }

    // Foreground run.
    let socket = cli.socket.clone().unwrap_or_else(ipn_ipc::default_socket);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve(dd, socket, async {
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

    // Guard against iroh's unbounded mapped-address cache (iroh#4293): restart the
    // process before its resident memory runs away into the OOM abort we captured.
    watchdog::spawn(data_dir.clone());

    // Leave the network while the machine sleeps, so a suspended laptop stops
    // announcing itself to the pool on every dark wake.
    power::spawn(engine.clone());

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

/// Serve one client connection, **cancelling the in-flight request if the client
/// goes away**. Without that, a `^C`'d or hung-up client left the request running
/// to completion against a socket nobody would ever read — a request that blocks
/// for minutes (as `SetRelays` used to) stranded the task, and the daemon only
/// noticed the dead socket when it finally tried to write the reply.
///
/// The reader gets its own task rather than being raced in the `select!` below:
/// `read_frame` reads a length prefix and then the body, so cancelling it
/// mid-frame would swallow bytes and desync the stream. Here it is never
/// cancelled, and its channel closing *is* the "client hung up" signal.
async fn handle_conn(engine: Engine, stream: transport::Stream) -> Result<()> {
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(8);
    let reader_task = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            if tx.send(frame).await.is_err() {
                break; // handler gone
            }
        }
        // EOF or a read error: dropping `tx` closes `rx`, which is how the
        // handler below learns the client is gone.
    });

    // Frames that arrived while an earlier request was still being handled.
    let mut queued: std::collections::VecDeque<Frame> = std::collections::VecDeque::new();
    loop {
        let frame = match queued.pop_front() {
            Some(f) => f,
            None => match rx.recv().await {
                Some(f) => f,
                None => break,
            },
        };
        let Message::Request(req) = frame.body else {
            continue;
        };

        if matches!(req, IpcRequest::Subscribe) {
            let st = engine.status().await.ok();
            send_event(&writer, IpcEvent::Status(st)).await?;
            subscribe_loop(&engine, &writer).await;
            break;
        }

        let work = handle_request(&engine, req);
        tokio::pin!(work);
        let resp = loop {
            tokio::select! {
                resp = &mut work => break Some(resp),
                next = rx.recv() => match next {
                    // A pipelined request; keep it for the next turn of the loop.
                    Some(f) => queued.push_back(f),
                    // The client hung up. Dropping `work` cancels it.
                    None => break None,
                },
            }
        };
        let Some(resp) = resp else {
            tracing::debug!("client disconnected mid-request; cancelled it");
            break;
        };

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

    reader_task.abort();
    Ok(())
}

/// Forward engine events to one subscriber, **coalescing status pushes**: a
/// burst of `Changed` events (roster fold + several connection observations can
/// land together) becomes a single `Status` push once the stream has been quiet
/// for [`STATUS_QUIET_MS`]. Join events are time-critical UX (the user is
/// staring at an emoji code) and are forwarded immediately, without flushing or
/// discarding a pending status. A `Lagged` receiver just means we missed some
/// `Changed`s — mark a status pending rather than killing the subscription
/// (which previously left the client event-less until it reconnected).
async fn subscribe_loop(
    engine: &Engine,
    writer: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<transport::Stream>>>,
) {
    use ipn_core::EngineEvent as E;
    use tokio::sync::broadcast::error::RecvError;

    const STATUS_QUIET_MS: u64 = 250;

    let mut ev = engine.subscribe();
    let mut pending_status = false;
    loop {
        let next = if pending_status {
            match tokio::time::timeout(
                std::time::Duration::from_millis(STATUS_QUIET_MS),
                ev.recv(),
            )
            .await
            {
                // Quiet window elapsed — flush one coalesced status.
                Err(_) => {
                    pending_status = false;
                    tracing::debug!("pushing coalesced status");
                    let st = engine.status().await.ok();
                    if send_event(writer, IpcEvent::Status(st)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Ok(r) => r,
            }
        } else {
            ev.recv().await
        };
        match next {
            Ok(E::JoinSas { sas }) => {
                if send_event(writer, IpcEvent::JoinSas { sas }).await.is_err() {
                    return;
                }
            }
            Ok(E::JoinRequest {
                node_id,
                hostname,
                sas,
            }) => {
                let ipc = IpcEvent::JoinRequest {
                    node_id,
                    hostname,
                    sas,
                };
                if send_event(writer, ipc).await.is_err() {
                    return;
                }
            }
            // Changed, and the Android-only TUN coordination events (never
            // emitted by the desktop daemon): all just mean "status is stale".
            Ok(E::Changed | E::TunSetupRequired { .. } | E::TunTeardownRequired) => {
                pending_status = true;
            }
            Err(RecvError::Lagged(_)) => pending_status = true,
            Err(RecvError::Closed) => return,
        }
    }
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

/// How long a relay probe waits for the relay to accept it. Generous: a relay
/// that is up but slow to answer must not be reported as a bad token.
const PROBE_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

async fn handle_request(engine: &Engine, req: IpcRequest) -> IpcResponse {
    use std::net::Ipv4Addr;
    let to_err = |e: anyhow::Error| IpcResponse::Err(format!("{e:#}"));
    match req {
        IpcRequest::Hello { .. } => IpcResponse::Hello {
            version: ipn_ipc::PROTO_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        },
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
        IpcRequest::SetMemberRole { node_id, controller } => {
            match engine.set_member_role(&node_id, controller).await {
                Ok(()) => IpcResponse::Ok,
                Err(e) => to_err(e),
            }
        }
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
        IpcRequest::GetControllerTicket => match engine.controller_ticket().await {
            Ok(t) => IpcResponse::Ticket(t),
            Err(e) => to_err(e),
        },
        IpcRequest::SetPeerTicketSingleUse { on } => {
            match engine.set_peer_ticket_single_use(on).await {
                Ok(()) => IpcResponse::Ok,
                Err(e) => to_err(e),
            }
        }
        IpcRequest::SetRemoteAccessDisabled { disabled } => {
            match engine.set_remote_access_disabled(disabled).await {
                Ok(()) => IpcResponse::Ok,
                Err(e) => to_err(e),
            }
        }
        IpcRequest::SetHidden { hidden } => match engine.set_hidden(hidden).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::GetAuditLog => match engine.audit_log().await {
            Ok(log) => IpcResponse::AuditLog(log),
            Err(e) => to_err(e),
        },
        IpcRequest::SetNetworkName { name } => match engine.set_network_name(name).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::SetNickname { node_id, name } => {
            match engine.set_nickname(&node_id, name).await {
                Ok(()) => IpcResponse::Ok,
                Err(e) => to_err(e),
            }
        }
        IpcRequest::SetNote { node_id, note } => match engine.set_note(&node_id, note).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::ExportOriginatorKey => match engine.export_originator_key().await {
            Ok(code) => IpcResponse::Recovery(code),
            Err(e) => to_err(e),
        },
        IpcRequest::ImportOriginatorKey { code } => match engine.import_originator_key(&code).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        IpcRequest::GetRelays => IpcResponse::Relays(engine.relay_status()),
        IpcRequest::SetRelays { settings } => match engine.set_relay_settings(settings).await {
            Ok(()) => IpcResponse::Ok,
            Err(e) => to_err(e),
        },
        // Safe to await on the request path, unlike the relay calls next door:
        // the probe binds its own endpoint (bounded by its own timeout) and takes
        // no engine lock, so a relay that hangs up on it stalls nothing but this
        // one connection — which the client is deliberately waiting on anyway.
        IpcRequest::ProbeRelay { url, token } => {
            let server = ipn_core::relays::RelayServer { url, token };
            match ipn_core::relays::probe_relay(&server, PROBE_RELAY_TIMEOUT).await {
                Ok(()) => IpcResponse::Ok,
                Err(e) => to_err(e),
            }
        }
        IpcRequest::Subscribe => IpcResponse::Ok,
    }
}
