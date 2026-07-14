//! The headless **tray agent** (`nullgate --agent`): a lightweight, user-session
//! companion to the privileged daemon. It owns the system tray icon and all
//! desktop notifications, and it launches the GUI on demand.
//!
//! Why a separate process? The daemon runs as a *system service* (Windows
//! session 0 / a root systemd unit / a macOS LaunchDaemon), which is walled off
//! from the user's graphical session and so cannot draw a tray icon or post a
//! notification you'd see. The agent runs in the login session instead, talks to
//! the daemon over the same IPC socket the GUI uses, and — being independent of
//! the GUI window — keeps the tray alive whether the GUI is closed or has crashed.
//!
//! It builds no window; `hold()` keeps the GApplication running with zero windows.
//! Its application id differs from the GUI's so both can be primary GApplications
//! at once (the agent autostarts at login; the GUI is launched on demand). The
//! tray's three actions: **Open Nullgate** (launch the GUI), **Restart Nullgate
//! daemon** (elevate + bounce the service), **Quit Nullgate** (disconnect, then
//! quit the agent).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;
use ipn_ipc::transport::{self, read_frame, write_frame};
use ipn_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message, NetworkStatus};
use tokio::runtime::Handle;

use crate::actions::{self, Actions};
use crate::notify::{self, notify_newly_online, OPEN_GUI_ACTION};
use crate::tray::{self, TrayActions, TrayItem};

/// The agent's GApplication id — distinct from the GUI's [`crate::APP_ID`] so the
/// two can both be primary instances (autostarted agent + on-demand GUI).
const AGENT_APP_ID: &str = "io.github.steeb_k.Nullgate.Agent";

/// Messages from the IO side to the tray/notification side (GTK main thread).
enum AgentMsg {
    Status(Option<NetworkStatus>),
    JoinRequest { hostname: String },
    DaemonDown,
    /// The daemon came back on a newer app version (an auto-update was applied),
    /// so this agent is stale — relaunch to match. Non-Windows only; on Windows
    /// the installer's Restart Manager relaunches it (see `register_restart`).
    #[cfg_attr(windows, allow(dead_code))]
    UpdateApplied,
}

/// Hold the agent's `flock` for the life of the process, returning `false` if another
/// agent already holds it. See [`crate::macos_single_instance`] for why macOS needs
/// this at all: without it, every GUI start left behind another agent — and another
/// tray icon. Linux/Windows keep using GApplication's own single-instance path.
#[cfg(target_os = "macos")]
fn acquire_single_instance_lock() -> bool {
    use crate::macos_single_instance::{agent_lock_path, take_flock};

    match take_flock(&agent_lock_path()) {
        Ok(Some(lock)) => {
            // The lock lives exactly as long as its fd; hold it for the whole process.
            std::mem::forget(lock);
            true
        }
        Ok(None) => false,
        // Don't let a lock-file problem cost the user their tray icon.
        Err(e) => {
            tracing::warn!(error = %e, "agent: cannot open lock file; starting unguarded");
            true
        }
    }
}

/// Run the tray agent to completion. Returns when "Quit Nullgate" is chosen.
pub fn run(socket: PathBuf) -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    if !acquire_single_instance_lock() {
        tracing::info!("agent: another tray agent already holds the lock; exiting");
        return glib::ExitCode::SUCCESS;
    }

    // A Tokio runtime on a side thread does the socket IO, mirroring the GUI.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        handle_tx.send(rt.handle().clone()).expect("send handle");
        rt.block_on(std::future::pending::<()>());
    });
    let handle = handle_rx.recv().expect("runtime handle");

    // Brand the process (notification app-name, taskbar identity) as "Nullgate".
    glib::set_prgname(Some("Nullgate"));
    glib::set_application_name("Nullgate");

    let app = adw::Application::builder().application_id(AGENT_APP_ID).build();
    // The icon theme (used for notification icons) is registered under APP_ID.
    let built = Cell::new(false);
    app.connect_activate(move |app| {
        // A second `--agent` launch just activates the primary; don't double-build.
        if built.replace(true) {
            return;
        }
        build_agent(app, handle.clone(), socket.clone());
    });
    let empty: [&str; 0] = [];
    app.run_with_args(&empty)
}

fn build_agent(app: &adw::Application, handle: Handle, socket: PathBuf) {
    // No window: hold the application so it keeps running with zero windows. The
    // guard releases on drop, so keep it alive for the whole process — `app.quit()`
    // (from "Quit Nullgate") still terminates regardless of the outstanding hold.
    std::mem::forget(app.hold());
    crate::install_app_icon();
    #[cfg(windows)]
    notify::init_windows_app_id();

    // Action targeted by a notification's default click + "Open Nullgate" button
    // (Linux/macOS). On Windows the toast opens the GUI from its own callback.
    {
        let action = gtk::gio::SimpleAction::new(OPEN_GUI_ACTION, None);
        action.connect_activate(|_, _| crate::launch_gui());
        app.add_action(&action);
    }

    // The last status snapshot: the notification logic's baseline, the source of the
    // tray's device names, and what an action's placeholders expand against.
    let state: Rc<RefCell<Option<NetworkStatus>>> = Default::default();
    // The per-device actions file. The GUI is a *different process*, so this is
    // re-read whenever the file changes (see the mtime poll below).
    let device_actions: Rc<RefCell<Actions>> = Rc::new(RefCell::new(Actions::load()));

    // Tray → four action channels, and a stream of device-section updates back.
    let (open_tx, open_rx) = async_channel::unbounded::<()>();
    let (restart_tx, restart_rx) = async_channel::unbounded::<()>();
    let (quit_tx, quit_rx) = async_channel::unbounded::<()>();
    let (run_tx, run_rx) = async_channel::unbounded::<String>();
    let (items_tx, items_rx) = async_channel::unbounded::<Vec<TrayItem>>();
    tray::install(
        TrayActions {
            open: open_tx,
            restart_daemon: restart_tx,
            quit: quit_tx,
            run_action: run_tx,
        },
        items_rx,
    );

    // Recompute the tray's device section and push it, but only when it actually
    // changed — on Linux each push is a D-Bus menu-layout signal, and status
    // snapshots arrive far more often than anyone edits an action.
    let push_items = {
        let state = state.clone();
        let device_actions = device_actions.clone();
        let last: RefCell<Vec<TrayItem>> = RefCell::new(Vec::new());
        move || {
            let items = tray_items(state.borrow().as_ref(), &device_actions.borrow());
            if *last.borrow() != items {
                *last.borrow_mut() = items.clone();
                let _ = items_tx.try_send(items);
            }
        }
    };
    let push_items = Rc::new(push_items);

    // The actions file is written by the GUI process, so poll its mtime: one `stat`
    // a second is cheaper than a filesystem-watcher dependency, and the tray picking
    // an edit up a beat late is invisible.
    {
        let device_actions = device_actions.clone();
        let push_items = push_items.clone();
        let mut seen = actions::mtime_ms();
        glib::timeout_add_seconds_local(1, move || {
            let now = actions::mtime_ms();
            if now != seen {
                seen = now;
                *device_actions.borrow_mut() = Actions::load();
                push_items();
            }
            glib::ControlFlow::Continue
        });
    }

    // A tray device entry → spawn that device's command. The agent has no window to
    // toast into, so a failure becomes a desktop notification.
    {
        let app = app.clone();
        let state = state.clone();
        let device_actions = device_actions.clone();
        glib::spawn_future_local(async move {
            while let Ok(node_id) = run_rx.recv().await {
                // Look both up at click time: the menu entry may predate the latest
                // status (or an edit to the action itself) by a tick.
                let member = state
                    .borrow()
                    .as_ref()
                    .and_then(|s| s.members.iter().find(|m| m.node_id == node_id).cloned());
                let action = device_actions.borrow().get(&node_id).cloned();
                let (Some(member), Some(action)) = (member, action) else {
                    continue;
                };
                if let Err(e) = actions::run(&action, &member) {
                    notify::notify(
                        &app,
                        &format!("Couldn't run “{}”", action.label),
                        Some(&e.to_string()),
                    );
                }
            }
        });
    }

    // Open Nullgate → launch (or focus) the GUI window.
    glib::spawn_future_local(async move {
        while open_rx.recv().await.is_ok() {
            crate::launch_gui();
        }
    });

    // Restart Nullgate daemon → run the elevated restart off the GTK thread; on
    // failure, surface a desktop notification (the agent has no toast overlay).
    {
        let handle = handle.clone();
        let app = app.clone();
        glib::spawn_future_local(async move {
            while restart_rx.recv().await.is_ok() {
                let (res_tx, res_rx) = async_channel::bounded::<Result<(), String>>(1);
                handle.spawn(async move {
                    let r = tokio::task::spawn_blocking(crate::service_ctl::restart_daemon_service)
                        .await
                        .unwrap_or_else(|_| Err("Couldn't launch the elevation prompt.".into()));
                    let _ = res_tx.send(r).await;
                });
                if let Ok(Err(e)) = res_rx.recv().await {
                    notify::notify(&app, "Couldn't restart the Nullgate daemon", Some(&e));
                }
            }
        });
    }

    // Quit Nullgate → disconnect from the network, then quit the agent. The daemon
    // service keeps running; only this user-session process exits.
    {
        let app = app.clone();
        let handle = handle.clone();
        let socket_q = socket.clone();
        glib::spawn_future_local(async move {
            while quit_rx.recv().await.is_ok() {
                let (done_tx, done_rx) = async_channel::bounded::<()>(1);
                let socket = socket_q.clone();
                handle.spawn(async move {
                    let _ = transport::oneshot_request(&socket, IpcRequest::Disconnect).await;
                    let _ = done_tx.send(()).await;
                });
                let _ = done_rx.recv().await;
                app.quit();
            }
        });
    }

    // Long-lived subscription to daemon events, reconnecting if it restarts.
    let (tx, rx) = async_channel::unbounded::<AgentMsg>();
    {
        let socket = socket.clone();
        handle.spawn(async move {
            loop {
                let _ = stream_events(&socket, &tx).await;
                let _ = tx.send(AgentMsg::DaemonDown).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    // Drive notifications (and the tray's device section) off the event stream.
    {
        let app = app.clone();
        let state = state.clone();
        let push_items = push_items.clone();
        glib::spawn_future_local(async move {
            let mut offline_since: HashMap<String, Instant> = HashMap::new();
            while let Ok(msg) = rx.recv().await {
                match msg {
                    AgentMsg::Status(Some(s)) => {
                        notify_newly_online(&app, state.borrow().as_ref(), &s, &mut offline_since);
                        *state.borrow_mut() = Some(s);
                        // Member names (and who exists at all) come from the status.
                        push_items();
                    }
                    // No baseline while disconnected, so a reconnect doesn't spuriously
                    // announce every peer as "came online".
                    AgentMsg::Status(None) | AgentMsg::DaemonDown => {
                        *state.borrow_mut() = None;
                        push_items();
                    }
                    AgentMsg::JoinRequest { hostname } => {
                        notify::notify(
                            &app,
                            &format!("“{hostname}” wants to join"),
                            Some("Open Nullgate to approve or deny."),
                        );
                    }
                    AgentMsg::UpdateApplied => {
                        crate::relaunch_agent();
                        app.quit();
                    }
                }
            }
        });
    }

    // Windows: register a Restart-Manager relaunch so an interactive MSI update
    // brings the agent back (as `--agent`) after swapping the binary.
    #[cfg(windows)]
    crate::register_agent_restart();
}

/// The tray's per-device section: one entry per member that has an action, read as
/// "workshop-pc (RDP)". Offline members keep their entry — Nullgate can't know
/// whether a given command needs the peer up (a wake-on-LAN certainly doesn't).
///
/// Ordered by the text itself so the menu doesn't reshuffle under the cursor when a
/// member goes online and the status list reorders.
fn tray_items(status: Option<&NetworkStatus>, actions: &Actions) -> Vec<TrayItem> {
    let Some(status) = status else {
        return Vec::new();
    };
    let mut items: Vec<TrayItem> = status
        .members
        .iter()
        .filter_map(|m| {
            let action = actions.get(&m.node_id)?;
            let name = m
                .label
                .clone()
                .or_else(|| m.hostname.clone())
                .unwrap_or_else(|| m.node_id.chars().take(8).collect());
            Some(TrayItem {
                node_id: m.node_id.clone(),
                text: format!("{name} ({})", action.label),
            })
        })
        .collect();
    items.sort_by(|a, b| a.text.cmp(&b.text));
    items
}

#[cfg(test)]
mod tray_item_tests {
    use ipn_ipc::{MemberView, NetworkStatus};

    use super::tray_items;
    use crate::actions::{ActionColor, Actions, DeviceAction};

    fn member(node_id: &str, label: &str, online: bool) -> MemberView {
        MemberView {
            node_id: node_id.into(),
            hostname: Some("host".into()),
            label: Some(label.into()),
            note: None,
            virtual_ip: Some("10.99.0.5".into()),
            local_ip: None,
            public_ip: None,
            location: None,
            observed_addr: None,
            direct: None,
            online,
            last_seen: 0,
            is_self: false,
            is_originator_device: false,
            role: "peer".into(),
            access_disabled: false,
            hidden: false,
        }
    }

    fn status(members: Vec<MemberView>) -> NetworkStatus {
        NetworkStatus {
            name: "net".into(),
            subnet: "10.99.0.0".into(),
            frozen: false,
            self_node_id: "self".into(),
            self_ip: None,
            is_originator: false,
            self_role: "peer".into(),
            peer_ticket_single_use: false,
            routing: true,
            online: true,
            home_relay: None,
            members,
        }
    }

    fn action(label: &str) -> DeviceAction {
        DeviceAction {
            label: label.into(),
            color: ActionColor::Blue,
            command: "prog {ip}".into(),
            terminal: false,
        }
    }

    #[test]
    fn only_members_with_an_action_appear_and_read_name_paren_label() {
        let mut cfg = Actions::default();
        cfg.set("aaa", action("RDP"));
        let items = tray_items(
            Some(&status(vec![
                member("aaa", "workshop-pc", true),
                member("bbb", "no-action-box", true),
            ])),
            &cfg,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "workshop-pc (RDP)");
        assert_eq!(items[0].node_id, "aaa");
    }

    /// An offline device keeps its entry: the command may not need the peer up.
    #[test]
    fn offline_members_keep_their_entry_and_order_is_stable() {
        let mut cfg = Actions::default();
        cfg.set("aaa", action("RDP"));
        cfg.set("bbb", action("SSH"));
        let items = tray_items(
            Some(&status(vec![
                member("bbb", "zulu", false),
                member("aaa", "alpha", true),
            ])),
            &cfg,
        );
        // Sorted by text, NOT by the status order (which reshuffles on presence).
        assert_eq!(
            items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["alpha (RDP)", "zulu (SSH)"]
        );
    }

    #[test]
    fn no_status_means_no_entries() {
        let mut cfg = Actions::default();
        cfg.set("aaa", action("RDP"));
        assert!(tray_items(None, &cfg).is_empty());
    }
}

/// Subscribe to the daemon's event stream, forwarding the events the agent cares
/// about (status for online/offline alerts; join requests) as [`AgentMsg`]s.
async fn stream_events(socket: &Path, tx: &async_channel::Sender<AgentMsg>) -> std::io::Result<()> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Handshake first, so an applied auto-update (daemon back on a newer version)
    // relaunches this agent to match, keeping the tray from running stale code.
    write_frame(
        &mut writer,
        &Frame {
            id: 2,
            body: Message::Request(IpcRequest::Hello {
                version: ipn_ipc::PROTO_VERSION,
            }),
        },
    )
    .await?;
    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            return Ok(());
        };
        if let Message::Response(IpcResponse::Hello {
            version,
            app_version,
        }) = frame.body
        {
            // A protocol mismatch means the daemon was updated under us; relaunch to
            // match on non-Windows (Windows is restarted by the installer instead).
            if version != ipn_ipc::PROTO_VERSION {
                #[cfg(not(windows))]
                let _ = tx.send(AgentMsg::UpdateApplied).await;
                return Ok(());
            }
            #[cfg(not(windows))]
            if !app_version.is_empty() && app_version != env!("CARGO_PKG_VERSION") {
                let _ = tx.send(AgentMsg::UpdateApplied).await;
                return Ok(());
            }
            #[cfg(windows)]
            let _ = &app_version;
            break;
        }
    }

    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: Message::Request(IpcRequest::Subscribe),
        },
    )
    .await?;
    while let Some(frame) = read_frame(&mut reader).await? {
        if let Message::Event(ev) = frame.body {
            match ev {
                IpcEvent::Status(s) => {
                    let _ = tx.send(AgentMsg::Status(s)).await;
                }
                IpcEvent::JoinRequest { hostname, .. } => {
                    let _ = tx.send(AgentMsg::JoinRequest { hostname }).await;
                }
                // JoinSas is only meaningful to the joining device's approval UI.
                IpcEvent::JoinSas { .. } => {}
            }
        }
    }
    Ok(())
}
