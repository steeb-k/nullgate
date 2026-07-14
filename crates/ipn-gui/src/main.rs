// Release Windows builds are a GUI-subsystem binary so launching the app doesn't
// pop a console window. Debug keeps the console for dev logging.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Nullgate desktop GUI (GTK4 + libadwaita) — an **unprivileged IPC
//! client** to `ipn-daemon`. The daemon owns the iroh node + TUN (the only thing
//! needing elevation); this process just renders state and sends commands, so it
//! never needs admin/root.
//!
//! Threading: a Tokio runtime on a side thread does the socket IO; results and
//! pushed events arrive on the GTK main thread via an `async-channel` consumed by
//! `glib::spawn_future_local`. GTK objects are only touched on the main thread.
//!
//! Layout (SEED-style): a static "Nullgate" titlebar; a main page with a control group
//! (Administration, Show join ticket, Diagnostics) and a Members list at the
//! bottom (this device included). Each control row, and each member, opens a
//! slide-in **flyout** — an `adw::OverlaySplitView` sidebar that overlays the
//! content (the window stays visible behind it), so it reads as a sub-menu.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use ipn_ipc::transport::{self, read_frame, write_frame};
use ipn_ipc::{
    AuditEntry, Frame, IpcEvent, IpcRequest, IpcResponse, MemberView, Message, NetworkStatus,
    RelayApply, RelayPolicy, RelayServer, RelaySettings, RelayStatus,
};
use tokio::runtime::Handle;

mod agent;
mod notify;
mod service_ctl;
mod tray;

pub(crate) const APP_ID: &str = "io.github.steeb_k.Nullgate";

/// Single-instance support for macOS, where GApplication's own machinery is inert.
///
/// GLib implements "a second launch just activates the first" over a D-Bus session
/// bus, and macOS has none — so `g_application_register` never finds a primary and
/// every launch becomes one. Both long-lived roles rebuild the guarantee here on top
/// of `flock`: the tray agent (exactly one tray icon) and the GUI (exactly one
/// window). `open -a` is not an option for either — the agent runs the bundle's
/// `CFBundleExecutable`, so Launch Services thinks the app is already running and
/// activates the *headless agent* instead of opening a window.
#[cfg(target_os = "macos")]
pub(crate) mod macos_single_instance {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn uid() -> u32 {
        // SAFETY: `getuid` is always safe; it reads the calling process's real uid.
        unsafe { libc::getuid() }
    }

    /// Paths are per-uid: each user's Aqua session runs its own agent and GUI.
    fn gui_lock_path() -> String {
        format!("/tmp/nullgate-gui-{}.lock", uid())
    }
    fn gui_sock_path() -> String {
        format!("/tmp/nullgate-gui-{}.sock", uid())
    }
    pub(crate) fn agent_lock_path() -> String {
        format!("/tmp/nullgate-agent-{}.lock", uid())
    }

    /// Take an exclusive, non-blocking `flock`. `Ok(None)` means another process holds
    /// it. The kernel drops the lock when the fd closes, so — unlike a pidfile — it
    /// cannot survive a crash. Callers must keep the returned `File` alive.
    pub(crate) fn take_flock(path: &str) -> io::Result<Option<File>> {
        let file = OpenOptions::new().create(true).write(true).mode(0o600).open(path)?;
        // SAFETY: `file` owns a valid open fd for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(None);
        }
        Ok(Some(file))
    }

    /// Outcome of trying to become the one GUI process.
    pub(crate) enum GuiSlot {
        /// This process owns the window. The listener, when present, receives
        /// "present yourself" pokes from later launches.
        Primary(Option<UnixListener>),
        /// A window is already open and has been asked to come forward; exit quietly.
        AlreadyOpen,
    }

    pub(crate) fn claim_gui_slot() -> GuiSlot {
        let lock = match take_flock(&gui_lock_path()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                // Someone owns the window: poke them, then step aside. A failed connect
                // is not worth blocking on — the primary is up either way.
                let _ = UnixStream::connect(gui_sock_path());
                return GuiSlot::AlreadyOpen;
            }
            // A lock-file problem must never cost the user their window.
            Err(e) => {
                tracing::warn!(error = %e, "gui: cannot open lock file; starting unguarded");
                return GuiSlot::Primary(None);
            }
        };
        // The lock lives exactly as long as its fd; hold it for the whole process.
        std::mem::forget(lock);
        // We hold the lock, so any socket here is a leftover from a dead primary.
        let _ = std::fs::remove_file(gui_sock_path());
        match UnixListener::bind(gui_sock_path()) {
            Ok(listener) => GuiSlot::Primary(Some(listener)),
            Err(e) => {
                tracing::warn!(error = %e, "gui: cannot bind present socket; window won't be reusable");
                GuiSlot::Primary(None)
            }
        }
    }
}

/// Messages from the IO side to the UI.
#[derive(Clone)]
enum UiMsg {
    Status(Option<NetworkStatus>),
    Ticket(String),
    JoinSas(Vec<String>),
    JoinRequest {
        node_id: String,
        hostname: String,
        sas: Vec<String>,
    },
    Recovery(String),
    /// The administration activity log to display in its flyout.
    AuditLog(Vec<AuditEntry>),
    Toast(String),
    /// The device's relay settings + apply state, to render in the "Relay
    /// servers" flyout.
    Relays(RelayStatus),
    /// A relay-settings write was accepted; re-fetch so the open panel shows the
    /// state the daemon actually reached (sequenced after the write, unlike a
    /// parallel GetRelays).
    RelaysSaved,
    /// Re-render the current status (e.g. after a pending-join change).
    Refresh,
    DaemonDown,
    VersionMismatch { daemon: u32, gui: u32 },
    /// The daemon came back on a newer app version (an auto-update was applied),
    /// so this GUI is stale — relaunch to match. Linux/macOS only; on Windows the
    /// installer's Restart Manager closes + replaces + restarts the GUI instead.
    #[cfg_attr(windows, allow(dead_code))]
    UpdateApplied,
}

/// A join request awaiting the user's decision, kept so it survives a missed/
/// dismissed prompt and can be approved later from the main window.
#[derive(Clone)]
struct PendingJoin {
    node_id: String,
    hostname: String,
    sas: Vec<String>,
}

/// Everything needed to fire IPC requests off the GTK thread.
#[derive(Clone)]
struct Net {
    handle: Handle,
    socket: PathBuf,
    tx: async_channel::Sender<UiMsg>,
}

impl Net {
    /// Fire a request on the runtime and deliver a mapped [`UiMsg`] to the UI.
    fn request<F>(&self, req: IpcRequest, map: F)
    where
        F: FnOnce(std::io::Result<IpcResponse>) -> Option<UiMsg> + Send + 'static,
    {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let res = transport::oneshot_request(&socket, req).await;
            if let Some(msg) = map(res) {
                let _ = tx.send(msg).await;
            }
        });
    }

    /// Fire a request and hand the response back **on the GTK thread**, so the
    /// caller can update the widget that asked for it.
    ///
    /// [`request`](Self::request) maps its response on the runtime, which cannot
    /// touch GTK objects — everything it learns has to fit through a [`UiMsg`]
    /// and be rendered globally. That is the right shape for state the whole page
    /// shows, and the wrong one for an answer that belongs to a single dialog or
    /// row (a relay probe: "did *this* relay take *this* token?").
    fn request_local<F>(&self, req: IpcRequest, done: F)
    where
        F: FnOnce(std::io::Result<IpcResponse>) + 'static,
    {
        let socket = self.socket.clone();
        let (tx, rx) = async_channel::bounded(1);
        self.handle.spawn(async move {
            let _ = tx.send(transport::oneshot_request(&socket, req).await).await;
        });
        glib::spawn_future_local(async move {
            if let Ok(res) = rx.recv().await {
                done(res);
            }
        });
    }

    /// Push a transient toast to the UI from the GTK thread (synchronous callers).
    fn toast(&self, msg: impl Into<String>) {
        let _ = self.tx.try_send(UiMsg::Toast(msg.into()));
    }

    /// (Re)start the privileged daemon service, prompting for elevation. The
    /// blocking OS auth prompt runs on the runtime (never the GTK thread); success
    /// is left to the reconnect loop to notice, so only failures toast back.
    fn restart_service(&self) {
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match tokio::task::spawn_blocking(crate::service_ctl::restart_daemon_service).await {
                Ok(Ok(())) => {} // reconnect loop clears the banner when the daemon returns
                Ok(Err(e)) => {
                    let _ = tx.send(UiMsg::Toast(e)).await;
                }
                Err(_) => {
                    let _ = tx
                        .send(UiMsg::Toast("Couldn't launch the elevation prompt.".into()))
                        .await;
                }
            }
        });
    }

    /// Ask the UI to re-render the current status.
    fn refresh(&self) {
        let _ = self.tx.try_send(UiMsg::Refresh);
    }

    /// Long-lived subscription to daemon events, reconnecting if it restarts.
    fn subscribe_loop(&self) {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            loop {
                let _ = stream_events(&socket, &tx).await;
                let _ = tx.send(UiMsg::DaemonDown).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }
}

async fn stream_events(socket: &std::path::Path, tx: &async_channel::Sender<UiMsg>) -> std::io::Result<()> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Version handshake first: a GUI/daemon mismatch is surfaced clearly instead
    // of failing on an unknown message later.
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
            if version != ipn_ipc::PROTO_VERSION {
                let _ = tx
                    .send(UiMsg::VersionMismatch {
                        daemon: version,
                        gui: ipn_ipc::PROTO_VERSION,
                    })
                    .await;
                return Ok(());
            }
            // After an auto-update the daemon restarts on a newer app version while
            // this GUI is still the old binary; relaunch ourselves to match (the
            // binary was swapped in place). On Windows the GUI can't self-relaunch —
            // its exe is locked and the SYSTEM updater lives in another session — so
            // it's restarted externally instead (see `restart_self`/`register_restart`).
            #[cfg(not(windows))]
            if !app_version.is_empty() && app_version != env!("CARGO_PKG_VERSION") {
                let _ = tx.send(UiMsg::UpdateApplied).await;
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
            let msg = match ev {
                IpcEvent::Status(s) => UiMsg::Status(s),
                IpcEvent::JoinSas { sas } => UiMsg::JoinSas(sas),
                IpcEvent::JoinRequest {
                    node_id,
                    hostname,
                    sas,
                } => UiMsg::JoinRequest {
                    node_id,
                    hostname,
                    sas,
                },
            };
            let _ = tx.send(msg).await;
        }
    }
    Ok(())
}

/// Install the app stylesheet: a base "frameless" look on every platform, plus a
/// Windows 11-leaning layer (Segoe UI, accent, rounding) on Windows. (Borrowed
/// from seed-sync-gtk; macOS has no extra sheet there either.)
/// Register the bundled app icon (`nullgate-icon-*.png`) into the icon theme under
/// `APP_ID`, so `application_icon(APP_ID)` resolves on every platform (Windows'
/// GTK theme has no entry for it otherwise). Also sets it as the default window
/// icon. Best-effort.
fn install_app_icon() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let Some(dirs) = directories::BaseDirs::new() else {
        return;
    };
    // Windows: %LOCALAPPDATA%\ipn\icons ; Linux: ~/.cache/ipn/icons. Register the
    // per-size app icon so the window/taskbar icon is crisp at each size (16 and 32
    // are downscaled from the 1024 master; 64+ are the artist's sizes). Always
    // (over)write so a replaced asset takes effect next launch.
    let base = dirs.cache_dir().join("nullgate").join("icons");
    let sizes: [(&str, &[u8]); 6] = [
        ("16x16", include_bytes!("../../../img/nullgate-icon-16.png")),
        ("32x32", include_bytes!("../../../img/nullgate-icon-32.png")),
        ("64x64", include_bytes!("../../../img/nullgate-icon-64.png")),
        ("128x128", include_bytes!("../../../img/nullgate-icon-128.png")),
        ("256x256", include_bytes!("../../../img/nullgate-icon-256.png")),
        ("512x512", include_bytes!("../../../img/nullgate-icon-512.png")),
    ];
    for (size, bytes) in sizes {
        let apps = base.join("hicolor").join(size).join("apps");
        if std::fs::create_dir_all(&apps).is_ok() {
            let _ = std::fs::write(apps.join(format!("{APP_ID}.png")), bytes);
        }
    }
    gtk::IconTheme::for_display(&display).add_search_path(&base);
    gtk::Window::set_default_icon_name(APP_ID);
}

fn load_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    #[allow(unused_mut)]
    let mut css = String::from(include_str!("style.css"));
    #[cfg(windows)]
    css.push_str(include_str!("windows.css"));
    provider.load_from_data(&css);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Path of the small file remembering the window size (best-effort).
fn window_state_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("io.github", "steeb_k", "Nullgate")
        .map(|d| d.config_dir().join("gui-window"))
}

/// Load the saved window size as `(width, height)`, falling back to a sane default.
fn load_window_size() -> (i32, i32) {
    let parse = || -> Option<(i32, i32)> {
        let s = std::fs::read_to_string(window_state_path()?).ok()?;
        let (w, h) = s.trim().split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    };
    parse()
        .filter(|(w, h)| *w >= 360 && *h >= 360)
        .unwrap_or((560, 640))
}

/// Remember the current window size (best-effort; ignores errors).
fn save_window_size(window: &adw::ApplicationWindow) {
    let (w, h) = (window.width(), window.height());
    if w < 360 || h < 360 {
        return; // skip bogus sizes (e.g. while hidden)
    }
    if let Some(path) = window_state_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{w}x{h}"));
    }
}

/// On macOS, point GLib/GTK at the bundled runtime resources relative to this
/// executable, so the self-contained tarball install (GTK dylibs relocated to
/// `../lib`) finds its GSettings schemas, gdk-pixbuf loaders, and Adwaita icon
/// theme without a system/Homebrew/conda GTK. Every set is guarded by `exists()`,
/// so a dev build run against a system GTK (no bundled `share/`+`lib/` next to the
/// exe) is a no-op and keeps the system paths. Must run before any GLib/GTK call.
#[cfg(target_os = "macos")]
fn setup_runtime_env() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Installed binaries are reached through /usr/local/bin symlinks into the real
    // prefix, and current_exe() can hand back the symlink path — canonicalize so
    // the prefix resolves to the install root, not the symlink's parent. Without
    // this, the bundled share/lib/etc aren't found and GTK falls back to a system
    // prefix (absent on a user's machine → file-chooser / pixbuf crash).
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    // <prefix>/MacOS/nullgate -> prefix is Nullgate.app/Contents (holds lib/, share/, etc/).
    let Some(prefix) = exe.parent().and_then(|bin| bin.parent()) else {
        return;
    };
    let set_if = |var: &str, p: PathBuf| {
        if p.exists() && std::env::var_os(var).is_none() {
            std::env::set_var(var, &p);
        }
    };
    set_if("GSETTINGS_SCHEMA_DIR", prefix.join("share/glib-2.0/schemas"));
    set_if(
        "GDK_PIXBUF_MODULE_FILE",
        prefix.join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"),
    );
    set_if(
        "GDK_PIXBUF_MODULEDIR",
        prefix.join("lib/gdk-pixbuf-2.0/2.10.0/loaders"),
    );
    // fontconfig (pulled in by pango): the bundled libfontconfig has a compiled-in
    // config path under the build prefix, absent on a user's machine. Point it at
    // our bundled fonts.conf, which references the system macOS font dirs.
    set_if("FONTCONFIG_PATH", prefix.join("etc/fonts"));
    // Prepend our share/ so the bundled Adwaita icon theme is found by GTK.
    let share = prefix.join("share");
    if share.exists() {
        let val = match std::env::var_os("XDG_DATA_DIRS") {
            Some(cur) if !cur.is_empty() => {
                let mut s = std::ffi::OsString::from(&share);
                s.push(":");
                s.push(cur);
                s
            }
            _ => {
                let mut s = std::ffi::OsString::from(&share);
                s.push(":/usr/local/share:/usr/share");
                s
            }
        };
        std::env::set_var("XDG_DATA_DIRS", val);
    }
}

/// Non-macOS: system/MSI-installed GTK finds its own resources; nothing to set.
#[cfg(not(target_os = "macos"))]
fn setup_runtime_env() {}

/// Which of the binary's three roles to run. The same `nullgate` binary is the
/// GUI window, the headless tray agent (`--agent`), or a version probe.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Version,
    Agent,
    Gui,
}

fn parse_mode(args: &[String]) -> Mode {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        Mode::Version
    } else if args.iter().any(|a| a == "--agent") {
        Mode::Agent
    } else {
        Mode::Gui
    }
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mode = parse_mode(&args);
    if mode == Mode::Version {
        println!("nullgate {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    // Before any GLib/GTK call: on macOS, redirect GTK to the bundled resources.
    setup_runtime_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // The tray icon + desktop notifications live in a headless user-session agent,
    // separate from the GUI window, so they survive the GUI being closed/crashing.
    if mode == Mode::Agent {
        return agent::run(ipn_ipc::default_socket());
    }

    // macOS: GApplication can't dedupe us (no D-Bus session bus), so a second launch
    // — e.g. the tray's "Open Nullgate" while the window is already up — would build a
    // duplicate window. Hand the request to the existing window instead, and exit.
    #[cfg(target_os = "macos")]
    let present_requests = match macos_single_instance::claim_gui_slot() {
        macos_single_instance::GuiSlot::AlreadyOpen => return glib::ExitCode::SUCCESS,
        macos_single_instance::GuiSlot::Primary(listener) => listener,
    };

    // Whenever the GUI starts, make sure the tray agent is up. The agent is a
    // single-instance app, so this is a no-op if one is already running (e.g. from
    // login autostart); but it guarantees the tray appears the first time you open
    // Nullgate after an install, without waiting for the next login. The agent is a
    // separate process and keeps running after this GUI window is closed.
    //
    // Single-instance is enforced by GApplication on Windows/Linux, but on macOS
    // that runs over a D-Bus session bus which doesn't exist — so the agent takes an
    // `flock` there (see `macos_single_instance`). Without it, every GUI start left
    // behind another agent, and another tray icon.
    spawn_agent();

    #[cfg(windows)]
    notify::init_windows_app_id();

    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        handle_tx.send(rt.handle().clone()).expect("send handle");
        rt.block_on(std::future::pending::<()>());
    });
    let handle = handle_rx.recv().expect("runtime handle");

    let (tx, rx) = async_channel::unbounded::<UiMsg>();
    let net = Net {
        handle,
        socket: ipn_ipc::default_socket(),
        tx,
    };
    net.subscribe_loop();

    // Pin the GLib program/application name to the product brand. GTK otherwise
    // derives these from argv[0] (the binary name), and on Windows the program
    // name feeds the window-class / taskbar identity — so without this the running
    // process can surface under the crate codename instead of "Nullgate".
    glib::set_prgname(Some("Nullgate"));
    glib::set_application_name("Nullgate");

    let app = adw::Application::builder().application_id(APP_ID).build();
    #[cfg(target_os = "macos")]
    let present_requests = RefCell::new(present_requests);
    app.connect_activate(move |app| {
        // Re-launched (e.g. from the tray's "Open Nullgate", or the notification's
        // click action) while already running: single-instance activation just
        // presents the existing window rather than building a second one.
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        build_ui(app, net.clone(), rx.clone());
        // macOS only, and only once: start answering the pokes that later launches
        // send instead of opening a window of their own.
        #[cfg(target_os = "macos")]
        if let Some(listener) = present_requests.borrow_mut().take() {
            serve_present_requests(app, listener);
        }
    });
    let empty: [&str; 0] = [];
    app.run_with_args(&empty)
}

/// Present the window whenever another launch connects to our socket. Accepting
/// blocks, so it lives on its own thread and reaches GTK through an `async-channel`,
/// like every other off-thread event in this process.
#[cfg(target_os = "macos")]
fn serve_present_requests(app: &adw::Application, listener: std::os::unix::net::UnixListener) {
    let (tx, rx) = async_channel::unbounded::<()>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            // A failed accept says nothing about the next one; a closed channel means
            // the GUI is gone, so stop.
            if stream.is_ok() && tx.send_blocking(()).is_err() {
                break;
            }
        }
    });
    let app = app.clone();
    glib::spawn_future_local(async move {
        while rx.recv().await.is_ok() {
            if let Some(win) = app.active_window() {
                win.present();
            }
        }
    });
}

/// Handles to the persistent widgets, passed to the render functions.
#[derive(Clone)]
struct Ui {
    split: adw::OverlaySplitView,
    stack: gtk::Stack,
    main_box: gtk::Box,
    /// Full-width banner (a ToolbarView top bar, so it survives page rebuilds) for
    /// the service state: daemon down / offline / routing off, with a (re)start button.
    service_banner: adw::Banner,
    /// Full-width banner announcing pending join requests, with a Review button that
    /// opens the admin flyout's emoji-SAS approval screen.
    join_banner: adw::Banner,
    admin_box: gtk::Box,
    diag_box: gtk::Box,
    ticket_box: gtk::Box,
    member_box: gtk::Box,
    audit_box: gtk::Box,
    relays_box: gtk::Box,
    member_title: adw::WindowTitle,
    notes_view: gtk::TextView,
    notes_target: Rc<RefCell<Option<String>>>,
    /// Note text edited this session (NodeId hex → text), for instant re-display.
    notes_cache: Rc<RefCell<HashMap<String, String>>>,
    /// The Notes row of the currently-open member flyout, so saving can refresh its
    /// preview subtitle without rebuilding the flyout.
    notes_row: Rc<RefCell<Option<adw::ActionRow>>>,
    /// Panels drilled through, so Back steps back one level instead of closing.
    nav_stack: Rc<RefCell<Vec<String>>>,
}

impl Ui {
    /// Reveal a flyout by stack name (it overlays the content; window stays behind).
    /// If a flyout is already open, the current panel is pushed onto the history so
    /// Back returns to it.
    fn open(&self, name: &str) {
        if self.split.shows_sidebar() {
            if let Some(cur) = self.stack.visible_child_name() {
                if cur != name {
                    self.nav_stack.borrow_mut().push(cur.to_string());
                }
            }
        }
        self.stack.set_visible_child_name(name);
        self.split.set_show_sidebar(true);
    }
    /// Step back one panel, or close the flyout if we're at the first one.
    fn back(&self) {
        // Release the nav_stack borrow before `set_show_sidebar`, which fires the
        // show-sidebar handler (it borrows nav_stack too).
        let prev = self.nav_stack.borrow_mut().pop();
        match prev {
            Some(prev) => self.stack.set_visible_child_name(&prev),
            None => self.split.set_show_sidebar(false),
        }
    }
    fn close_flyout(&self) {
        self.nav_stack.borrow_mut().clear();
        self.split.set_show_sidebar(false);
    }
}

fn padded_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    b.set_margin_top(12);
    b.set_margin_bottom(12);
    b.set_margin_start(6);
    b.set_margin_end(6);
    b
}

fn build_ui(app: &adw::Application, net: Net, rx: async_channel::Receiver<UiMsg>) {
    load_css();
    install_app_icon();

    let (win_w, win_h) = load_window_size();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Nullgate")
        .default_width(win_w)
        .default_height(win_h)
        .build();

    // --- main header (static branding; carries the window controls) ---
    // "Nullgate" is the product name for this GUI client (codename ipn-gui).
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Nullgate",
        &format!("v{}", env!("CARGO_PKG_VERSION")),
    )));

    // "+" create/join — only shown when not in a network (toggled below).
    let add_btn = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Create or join a network")
        .build();
    let popover = gtk::Popover::new();
    let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    pop_box.set_margin_top(8);
    pop_box.set_margin_bottom(8);
    pop_box.set_margin_start(8);
    pop_box.set_margin_end(8);
    let create_btn = gtk::Button::with_label("Create a network");
    create_btn.add_css_class("flat");
    let join_btn = gtk::Button::with_label("Join with a ticket");
    join_btn.add_css_class("flat");
    pop_box.append(&create_btn);
    pop_box.append(&join_btn);
    popover.set_child(Some(&pop_box));
    add_btn.set_popover(Some(&popover));
    add_btn.set_visible(false); // shown only in the no-network state
    header.pack_start(&add_btn);

    // --- main page body (header + scrolling content) ---
    let main_box = padded_box();
    let clamp = adw::Clamp::builder().maximum_size(520).child(&main_box).build();
    let main_scroller = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
    let main_toolbar = adw::ToolbarView::new();
    main_toolbar.add_top_bar(&header);
    // Two full-width banners below the header (outside the Clamp, so they span the
    // window like other GNOME apps and aren't torn down by main_box rebuilds). Both
    // start hidden; the receiver reveals/updates them per state. They can stack.
    let service_banner = adw::Banner::builder().revealed(false).build();
    let join_banner = adw::Banner::builder().revealed(false).build();
    main_toolbar.add_top_bar(&service_banner);
    main_toolbar.add_top_bar(&join_banner);
    main_toolbar.set_content(Some(&main_scroller));

    // --- flyout: an overlay sidebar (kept collapsed → always overlays the content
    // with a scrim + slide, the window visible behind it). It's the TOP-LEVEL
    // widget so the flyout spans the full window height — over the header too,
    // like SEED — rather than sitting below it. A stack swaps which panel fills it. ---
    let split = adw::OverlaySplitView::new();
    split.set_collapsed(true);
    split.set_sidebar_position(gtk::PackType::Start);
    split.set_show_sidebar(false);
    split.set_min_sidebar_width(300.0);
    split.set_max_sidebar_width(460.0);
    split.set_sidebar_width_fraction(0.72);
    split.set_content(Some(&main_toolbar));

    let admin_box = padded_box();
    let diag_box = padded_box();
    let ticket_box = padded_box();
    let member_box = padded_box();
    let audit_box = padded_box();
    let relays_box = padded_box();

    // The flyout stack and a navigation history of the panels we drilled through,
    // so a panel's Back button steps back one level (e.g. member → notes → back →
    // member) instead of jumping straight to the main page.
    let stack = gtk::Stack::new();
    let nav_stack: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let go_back = {
        let split = split.clone();
        let stack = stack.clone();
        let nav_stack = nav_stack.clone();
        move || {
            // Pop into a local first: `set_show_sidebar(false)` synchronously fires
            // the show-sidebar handler (which borrows nav_stack), so the borrow must
            // be released before we touch the split — otherwise it double-borrows.
            let prev = nav_stack.borrow_mut().pop();
            match prev {
                Some(prev) => stack.set_visible_child_name(&prev),
                None => split.set_show_sidebar(false),
            }
        }
    };

    // Build a flyout panel (back-button top bar + scrollable content). Returns the
    // panel widget and its title widget (so dynamic titles can be updated).
    let make_panel = |title: &str, content: &gtk::Box| -> (adw::ToolbarView, adw::WindowTitle) {
        let tv = adw::ToolbarView::new();
        let hb = adw::HeaderBar::new();
        hb.set_show_start_title_buttons(false);
        hb.set_show_end_title_buttons(false);
        let wt = adw::WindowTitle::new(title, "");
        hb.set_title_widget(Some(&wt));
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .css_classes(["flat", "circular"])
            .build();
        {
            let go_back = go_back.clone();
            back.connect_clicked(move |_| go_back());
        }
        hb.pack_end(&back);
        tv.add_top_bar(&hb);
        let clamp = adw::Clamp::builder().maximum_size(520).child(content).build();
        let scr = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
        tv.set_content(Some(&scr));
        (tv, wt)
    };

    let (admin_panel, _) = make_panel("Administration", &admin_box);
    let (diag_panel, _) = make_panel("Diagnostics", &diag_box);
    let (ticket_panel, _) = make_panel("Join ticket", &ticket_box);
    let (member_panel, member_title) = make_panel("Member", &member_box);
    let (audit_panel, _) = make_panel("Activity log", &audit_box);
    let (relays_panel, _) = make_panel("Relay servers", &relays_box);

    // Notes panel: an editable text area presented as a rounded card that fills
    // the flyout below the header (with margins so it doesn't bleed to the edges)
    // and scrolls within itself (the text field scrolls, not the page).
    let notes_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    notes_view.add_css_class("notes-view");
    let notes_scroll = gtk::ScrolledWindow::builder()
        .child(&notes_view)
        .vexpand(true)
        .hexpand(true)
        .build();
    // Rounded "card" look, clipped to its corners — matches the rest of the app.
    notes_scroll.add_css_class("card");
    notes_scroll.set_overflow(gtk::Overflow::Hidden);
    let notes_outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    notes_outer.set_margin_top(12);
    notes_outer.set_margin_bottom(12);
    notes_outer.set_margin_start(12);
    notes_outer.set_margin_end(12);
    notes_outer.append(&notes_scroll);
    let notes_panel = adw::ToolbarView::new();
    {
        let hb = adw::HeaderBar::new();
        hb.set_show_start_title_buttons(false);
        hb.set_show_end_title_buttons(false);
        hb.set_title_widget(Some(&adw::WindowTitle::new("Notes", "")));
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .css_classes(["flat", "circular"])
            .build();
        let go_back = go_back.clone();
        back.connect_clicked(move |_| go_back());
        hb.pack_end(&back);
        notes_panel.add_top_bar(&hb);
    }
    notes_panel.set_content(Some(
        &adw::Clamp::builder().maximum_size(520).child(&notes_outer).build(),
    ));

    // Which member the open note belongs to (set when the flyout is opened).
    let notes_target: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Note text we've edited this session, keyed by NodeId hex. Written
    // synchronously on save and read when (re)opening the flyout, so the note
    // shows immediately — the status round-trip that rebuilds the member rows lags
    // a save by a tick, and the member flyout isn't rebuilt while it's open.
    let notes_cache: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let notes_row: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    // Autosave when focus leaves the text area (covers Back, scrim-dismiss, etc.).
    {
        let net2 = net.clone();
        let notes_target = notes_target.clone();
        let notes_cache = notes_cache.clone();
        let notes_row = notes_row.clone();
        let buffer = notes_view.buffer();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            let Some(node_id) = notes_target.borrow().clone() else {
                return;
            };
            let (s, e) = buffer.bounds();
            let text = buffer.text(&s, &e, false).to_string();
            // Record locally first (presence of the key means "edited this
            // session", even when cleared to empty), then persist via the daemon.
            notes_cache.borrow_mut().insert(node_id.clone(), text.clone());
            // Refresh the open member flyout's Notes preview immediately.
            if let Some(row) = notes_row.borrow().as_ref() {
                row.set_subtitle(&note_preview(Some(&text)));
            }
            let note = if text.trim().is_empty() { None } else { Some(text) };
            net2.request(IpcRequest::SetNote { node_id, note }, |_| None);
        });
        notes_view.add_controller(focus);
    }

    stack.add_named(&admin_panel, Some("admin"));
    stack.add_named(&diag_panel, Some("diagnostics"));
    stack.add_named(&ticket_panel, Some("ticket"));
    stack.add_named(&member_panel, Some("member"));
    stack.add_named(&audit_panel, Some("audit"));
    stack.add_named(&notes_panel, Some("notes"));
    stack.add_named(&relays_panel, Some("relays"));
    split.set_sidebar(Some(&stack));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&split));
    window.set_content(Some(&toast_overlay));

    let ui = Ui {
        split,
        stack,
        main_box,
        service_banner,
        join_banner,
        admin_box,
        diag_box,
        ticket_box,
        member_box,
        audit_box,
        relays_box,
        member_title,
        notes_view,
        notes_target,
        notes_cache,
        notes_row,
        nav_stack,
    };

    render_placeholder(&ui, &connecting_page());

    // Banner actions, wired once. The service banner's button (re)starts the daemon
    // (elevated); the join banner's button opens the emoji-SAS approval screen.
    {
        let net = net.clone();
        ui.service_banner.connect_button_clicked(move |_| {
            net.restart_service();
            net.toast("Starting the Nullgate service…");
        });
    }
    {
        let ui2 = ui.clone();
        ui.join_banner.connect_button_clicked(move |_| ui2.open("admin"));
    }

    {
        let net = net.clone();
        let window = window.clone();
        let popover = popover.clone();
        create_btn.connect_clicked(move |_| {
            popover.popdown();
            create_dialog(&window, &net);
        });
    }
    {
        let net = net.clone();
        let window = window.clone();
        let popover = popover.clone();
        join_btn.connect_clicked(move |_| {
            popover.popdown();
            join_dialog(&window, &net);
        });
    }

    let state: Rc<RefCell<Option<NetworkStatus>>> = Default::default();
    let pending: Rc<RefCell<Vec<PendingJoin>>> = Default::default();
    // The persistent main-page widget tree; every status is applied to it in
    // place (no teardown), so status pushes can't eat clicks or steal focus.
    let page = build_main_page(&ui, &net, &window);

    {
        let ui = ui.clone();
        let window = window.clone();
        let net = net.clone();
        let toast_overlay = toast_overlay.clone();
        let state = state.clone();
        let pending = pending.clone();
        let page = page.clone();
        let add_btn = add_btn.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::Status(Some(s)) => {
                        add_btn.set_visible(false); // already in a network
                        // Peer online/offline desktop notifications now come from the
                        // tray agent, so they fire even with this window closed.
                        pending
                            .borrow_mut()
                            .retain(|p| !s.members.iter().any(|m| m.node_id == p.node_id));
                        *state.borrow_mut() = Some(s.clone());
                        apply_status(&ui, &page, &s, &net, &window, &pending, &state);
                    }
                    UiMsg::Status(None) => {
                        add_btn.set_visible(true); // no network — offer create/join
                        *state.borrow_mut() = None;
                        page.reset();
                        set_service_banner(&ui, ServiceBanner::Hidden);
                        ui.join_banner.set_revealed(false);
                        render_placeholder(&ui, &empty_page(&net, &window));
                    }
                    UiMsg::Refresh => {
                        let st = state.borrow();
                        if let Some(s) = st.as_ref() {
                            apply_status(&ui, &page, s, &net, &window, &pending, &state);
                        }
                    }
                    UiMsg::DaemonDown => {
                        add_btn.set_visible(false);
                        *state.borrow_mut() = None;
                        page.reset();
                        set_service_banner(&ui, ServiceBanner::DaemonDown);
                        ui.join_banner.set_revealed(false);
                        render_placeholder(&ui, &daemon_down_page());
                    }
                    UiMsg::VersionMismatch { daemon, gui } => {
                        add_btn.set_visible(false);
                        *state.borrow_mut() = None;
                        page.reset();
                        set_service_banner(&ui, ServiceBanner::Hidden);
                        ui.join_banner.set_revealed(false);
                        render_placeholder(&ui, &version_mismatch_page(daemon, gui));
                    }
                    UiMsg::UpdateApplied => restart_self(&window),
                    UiMsg::Ticket(t) => fill_ticket(&ui, &t, &net, &window),
                    UiMsg::AuditLog(entries) => fill_audit(&ui, &entries),
                    UiMsg::Recovery(code) => show_recovery(&window, &net, &code),
                    UiMsg::JoinSas(sas) => show_join_sas(&window, &sas),
                    UiMsg::JoinRequest {
                        node_id,
                        hostname,
                        sas,
                    } => {
                        {
                            let mut p = pending.borrow_mut();
                            if !p.iter().any(|x| x.node_id == node_id) {
                                p.push(PendingJoin {
                                    node_id: node_id.clone(),
                                    hostname: hostname.clone(),
                                    sas,
                                });
                            }
                        }
                        // The "wants to join" desktop notification is raised by the
                        // tray agent; here we just surface it in the open window.
                        let st = state.borrow();
                        if let Some(s) = st.as_ref() {
                            apply_status(&ui, &page, s, &net, &window, &pending, &state);
                        }
                    }
                    UiMsg::Relays(status) => {
                        render_relays(&ui.relays_box, &status, &net, &window);
                        ui.open("relays");
                    }
                    UiMsg::RelaysSaved => {
                        // "Saved", not "applied" — the daemon is still pushing it
                        // into the live endpoint, and the panel we re-fetch below
                        // is what reports whether that actually landed.
                        toast_overlay.add_toast(adw::Toast::new("Relay settings saved"));
                        open_relays(&net);
                    }
                    UiMsg::Toast(t) => toast_overlay.add_toast(adw::Toast::new(&t)),
                }
            }
        });
    }

    // Re-render periodically so relative "last seen" times stay current.
    {
        let net = net.clone();
        glib::timeout_add_seconds_local(20, move || {
            net.refresh();
            glib::ControlFlow::Continue
        });
    }

    // Quit accelerator. The GUI is now a normal window: closing it (or this
    // shortcut) closes the window and quits the GUI process only — the daemon and
    // the tray agent keep running, so the network stays up. Ctrl+Q on Windows/Linux;
    // the standard Cmd+Q on macOS. Reopen from the tray's "Open Nullgate".
    {
        let action = gtk::gio::SimpleAction::new("quit", None);
        let w = window.clone();
        action.connect_activate(move |_, _| w.close());
        app.add_action(&action);
        #[cfg(not(target_os = "macos"))]
        app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
        #[cfg(target_os = "macos")]
        app.set_accels_for_action("app.quit", &["<Meta>q"]);
    }

    // "Back" navigation: Alt+Left, or Backspace (unless typing in a text field),
    // backs out of an open flyout to the main page.
    {
        let ui2 = ui.clone();
        let window2 = window.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_, keyval, _, state| {
            let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);
            let typing = adw::prelude::GtkWindowExt::focus(&window2)
                .is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
            let is_back = (alt && keyval == gtk::gdk::Key::Left)
                || (keyval == gtk::gdk::Key::BackSpace && !typing);
            if is_back && ui2.split.shows_sidebar() {
                ui2.back();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(key);
    }

    // If the flyout is dismissed any other way (scrim click / Escape), forget the
    // navigation history so the next drill-in starts clean.
    {
        let ui2 = ui.clone();
        ui.split.connect_show_sidebar_notify(move |s| {
            if !s.shows_sidebar() {
                ui2.nav_stack.borrow_mut().clear();
            }
        });
    }

    // Closing the window quits the GUI process (a normal app). The daemon + tray
    // agent keep running, so the network stays up; reopen from "Open Nullgate".
    window.connect_close_request(move |w| {
        save_window_size(w);
        glib::Propagation::Proceed
    });

    // Best-effort reconnect to a saved network; ignore errors (e.g. "no network" —
    // the empty screen already conveys that, so no toast).
    net.request(IpcRequest::Connect, |_| None);

    window.present();

    // Windows: register a Restart-Manager relaunch so an interactive MSI update
    // closes, replaces, and reopens the GUI window.
    #[cfg(windows)]
    register_restart_cmd(None);
}

/// Launch (or focus) the GUI window as a separate process — used by the tray's
/// "Open Nullgate" and by notification clicks. The GUI is a single-instance
/// GApplication, so if one is already open this just presents it. `current_exe`
/// with no arguments selects GUI mode (see [`parse_mode`]).
///
/// Note for macOS: this must spawn the executable directly. `open -a Nullgate.app`
/// looks like the idiomatic call, but the tray agent runs the bundle's
/// `CFBundleExecutable`, so Launch Services already counts the bundle as "running"
/// and merely sends an activate event to the headless agent — no window ever
/// appears. The duplicate-window problem that `open` would have solved is instead
/// handled by [`macos_single_instance`].
pub(crate) fn launch_gui() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Ensure the tray agent is running by spawning `nullgate --agent` as a detached
/// process. The agent is single-instance — a GApplication primary on Windows/Linux,
/// an `flock` holder on macOS — so if one is already running (from login autostart,
/// or a prior GUI start) the new process hands off to it and exits, making this safe
/// to call unconditionally. This is how the tray reliably appears whenever Nullgate
/// is used, without the user ever having to launch the agent by hand.
pub(crate) fn spawn_agent() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--agent").spawn();
    }
}

/// Relaunch this binary once the current process has exited, optionally with an
/// extra argument. A tiny `sh` shim waits for this PID first so a single-instance
/// GApplication doesn't collide with the outgoing instance. Linux/macOS only.
#[cfg(not(windows))]
fn relaunch_after_exit(extra_arg: Option<&str>) {
    if let Ok(exe) = std::env::current_exe() {
        let flag = extra_arg.map(|a| format!(" {a}")).unwrap_or_default();
        let script = format!(
            "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; exec \"{exe}\"{flag}",
            pid = std::process::id(),
            exe = exe.display(),
        );
        let _ = std::process::Command::new("sh").arg("-c").arg(script).spawn();
    }
}

/// Relaunch the GUI from disk (Linux/macOS), then exit so the new instance takes
/// over. Used after an auto-update swapped the binary in place.
#[cfg(not(windows))]
fn restart_self(_window: &adw::ApplicationWindow) {
    relaunch_after_exit(None);
    std::process::exit(0);
}

/// Relaunch the tray agent from disk (Linux/macOS) after an auto-update. Called by
/// the agent when the daemon returns on a newer version; the caller then quits.
#[cfg(not(windows))]
pub(crate) fn relaunch_agent() {
    relaunch_after_exit(Some("--agent"));
}

/// On Windows the GUI is restarted externally, so there's nothing to do here: the
/// SYSTEM auto-updater (`packaging/windows/nullgate-update.ps1`) relaunches it in
/// the user's session, and the installer's Restart Manager covers interactive MSI
/// runs (see `register_restart_cmd`).
#[cfg(windows)]
fn restart_self(_window: &adw::ApplicationWindow) {}

/// On Windows the agent is relaunched by the installer's Restart Manager (see
/// `register_agent_restart`), so self-relaunch is a no-op.
#[cfg(windows)]
pub(crate) fn relaunch_agent() {}

/// Register (or refresh) the Windows Restart Manager relaunch command line so an
/// **interactive** MSI run (elevated in the user's own session) closes, replaces,
/// and **restarts** the process — with `extra_arg` on the command line (e.g.
/// `--agent`). `RESTART_NO_CRASH | RESTART_NO_HANG` so it only relaunches for a
/// patch/reboot, not on a crash loop.
#[cfg(windows)]
fn register_restart_cmd(extra_arg: Option<&str>) {
    #[link(name = "kernel32")]
    extern "system" {
        fn RegisterApplicationRestart(pwz_commandline: *const u16, dw_flags: u32) -> i32;
    }
    unsafe {
        match extra_arg {
            Some(a) => {
                let args: Vec<u16> = a.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = RegisterApplicationRestart(args.as_ptr(), 0x1 | 0x2);
            }
            None => {
                let _ = RegisterApplicationRestart(std::ptr::null(), 0x1 | 0x2);
            }
        }
    }
}

/// Windows: register the tray agent for Restart-Manager relaunch as `--agent`.
#[cfg(windows)]
pub(crate) fn register_agent_restart() {
    register_restart_cmd(Some("--agent"));
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

/// Show a full-screen placeholder (connecting / empty / error), closing any flyout.
fn render_placeholder(ui: &Ui, page: &adw::StatusPage) {
    ui.close_flyout();
    clear_box(&ui.main_box);
    ui.main_box.append(page);
}

/// The service-state banner's condition (mutually exclusive at any moment).
enum ServiceBanner {
    Hidden,
    DaemonDown,
    Offline,
    RoutingOff,
}

/// Set the full-width service banner's message + button for the current condition.
/// The button is wired once (see `build_ui`) and always (re)starts the service.
fn set_service_banner(ui: &Ui, st: ServiceBanner) {
    let b = &ui.service_banner;
    match st {
        ServiceBanner::Hidden => b.set_revealed(false),
        ServiceBanner::DaemonDown => {
            b.set_title("The Nullgate service isn't running");
            b.set_button_label(Some("Start service"));
            b.set_revealed(true);
        }
        ServiceBanner::Offline => {
            b.set_title("Disconnected — the service lost its connection");
            b.set_button_label(Some("Restart service"));
            b.set_revealed(true);
        }
        ServiceBanner::RoutingOff => {
            b.set_title("Routing is off — traffic isn't being carried");
            b.set_button_label(Some("Restart service"));
            b.set_revealed(true);
        }
    }
}

/// Banner text for pending join requests, pluralized by count.
fn join_banner_text(n: usize) -> String {
    if n <= 1 {
        "A new device has requested network access".to_string()
    } else {
        format!("{n} devices have requested network access")
    }
}

fn connecting_page() -> adw::StatusPage {
    let spinner = gtk::Spinner::builder().width_request(32).height_request(32).build();
    spinner.start();
    adw::StatusPage::builder()
        .title("Connecting…")
        .description("Reaching the Nullgate background service.")
        .css_classes(["empty-state"])
        .child(&spinner)
        .vexpand(true)
        .build()
}

/// The body shown under the service banner while the daemon is down. The banner
/// above carries the message + Start button; this is just the ambient illustration.
fn daemon_down_page() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("network-error-symbolic")
        .description("Reconnects automatically once the service is running.")
        .css_classes(["empty-state"])
        .vexpand(true)
        .build()
}

fn version_mismatch_page(daemon: u32, gui: u32) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("Version mismatch")
        .description(format!(
            "The app (IPC v{gui}) and the background service (IPC v{daemon}) are different \
             versions. Update both Nullgate components to the same release."
        ))
        .css_classes(["empty-state"])
        .vexpand(true)
        .build()
}

fn empty_page(net: &Net, window: &adw::ApplicationWindow) -> adw::StatusPage {
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Center);
    let create = gtk::Button::with_label("Create a network");
    create.add_css_class("pill");
    create.add_css_class("suggested-action");
    let join = gtk::Button::with_label("Join with a ticket");
    join.add_css_class("pill");
    buttons.append(&create);
    buttons.append(&join);
    {
        let net = net.clone();
        let window = window.clone();
        create.connect_clicked(move |_| create_dialog(&window, &net));
    }
    {
        let net = net.clone();
        let window = window.clone();
        join.connect_clicked(move |_| join_dialog(&window, &net));
    }
    // Relay settings are device-level, so they're reachable (and worth setting)
    // before joining anything — a token-protected relay must be configured
    // before the join for the join traffic to use it.
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    outer.append(&buttons);
    let relays = gtk::Button::with_label("Relay servers…");
    relays.add_css_class("flat");
    relays.set_halign(gtk::Align::Center);
    {
        let net = net.clone();
        relays.connect_clicked(move |_| open_relays(&net));
    }
    outer.append(&relays);
    adw::StatusPage::builder()
        .icon_name("network-workgroup-symbolic")
        .title("No network yet")
        .description("Create a private network for your own devices, or join one with a ticket.")
        .css_classes(["empty-state"])
        .child(&outer)
        .vexpand(true)
        .build()
}

/// Title for a member's main-list row.
fn member_row_title(m: &MemberView) -> String {
    let mut title = m
        .label
        .clone()
        .or_else(|| m.hostname.clone())
        .unwrap_or_else(|| short_id(&m.node_id));
    if m.is_self {
        title.push_str(" (this device)");
    }
    title
}

/// Subtitle for a member's main-list row (host · ip · flags · path hint).
fn member_row_subtitle(m: &MemberView) -> String {
    let mut subtitle = String::new();
    if m.label.is_some() {
        if let Some(h) = &m.hostname {
            subtitle.push_str(h);
            subtitle.push_str(" · ");
        }
    }
    subtitle.push_str(&m.virtual_ip.clone().unwrap_or_else(|| "(no IP)".into()));
    // The access/hidden note sits next to the IP.
    if m.hidden {
        subtitle.push_str(" · Hidden");
    } else if m.access_disabled {
        subtitle.push_str(" · Access disabled");
    }
    // Online path hint only; "last seen" lives in the member detail flyout, and
    // the dot color already conveys offline/long-offline at a glance.
    if !m.is_self && m.online && !m.access_disabled {
        match m.direct {
            Some(true) => subtitle.push_str(" · direct"),
            Some(false) => subtitle.push_str(" · relay"),
            None => {}
        }
    }
    subtitle
}

/// Display order for the member list (node_id → position): the engine's
/// ranking (online → access-disabled → offline → hidden) with self pinned to
/// the top — but only when it's a normal online device; if self has disabled
/// access or hidden itself, it stays in its ranked spot.
fn member_order(members: &[MemberView]) -> HashMap<String, u32> {
    let mut ordered: Vec<&MemberView> = members.iter().collect();
    ordered.sort_by_key(|m| !(m.is_self && !m.access_disabled && !m.hidden));
    ordered
        .iter()
        .enumerate()
        .map(|(i, m)| (m.node_id.clone(), i as u32))
        .collect()
}

/// Everything the admin flyout displays. The flyout keeps a rebuild-style
/// renderer (its structure is role- and pending-dependent), so rebuilds are
/// gated on this signature. NOTHING volatile may appear here — per-tick
/// telemetry sneaking in is what used to recreate the Approve/Deny buttons
/// under the user's cursor.
fn admin_signature(s: &NetworkStatus, pending: &[PendingJoin]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = write!(
        out,
        "{}|{}|{}|{}|{}#",
        s.name, s.frozen, s.self_role, s.is_originator, s.peer_ticket_single_use,
    );
    for p in pending {
        out.push_str(&p.node_id);
        out.push(',');
    }
    out
}

/// One member's persistent row in the main list, updated in place on status.
struct MemberRowUi {
    row: adw::ActionRow,
    dot: gtk::Label,
}

/// The diagnostics flyout's persistent rows — updated in place because it's
/// the panel most likely to be open while connection telemetry churns.
struct DiagPanel {
    home_relay: adw::ActionRow,
    connections: adw::ActionRow,
    routing: adw::ActionRow,
}

/// The main page's persistent widget tree. Built ONCE per process;
/// [`apply_status`] mutates it in place. Widgets are never torn down on a
/// status push — destroying widgets mid-click (button-press landing on a row
/// that's gone before button-release) is why clicking often "didn't work".
struct MainPage {
    controls: adw::PreferencesGroup,
    pm_group: adw::PreferencesGroup,
    pm_controller_row: adw::ActionRow,
    members_group: adw::PreferencesGroup,
    members_list: gtk::ListBox,
    /// Live member rows by node_id (the diff target).
    rows: RefCell<HashMap<String, MemberRowUi>>,
    /// node_id → display position, read by the list's sort func.
    order: Rc<RefCell<HashMap<String, u32>>>,
    diag: DiagPanel,
    /// Gate for the admin flyout's rebuild (see [`admin_signature`]).
    last_admin_sig: RefCell<String>,
}

impl MainPage {
    /// Forget all member rows and gates. Called when the network goes away
    /// (leave / daemon down): a later join may be a *different* network, and
    /// stale rows must not survive into it.
    fn reset(&self) {
        for (_, r) in self.rows.borrow_mut().drain() {
            self.members_list.remove(&r.row);
        }
        self.order.borrow_mut().clear();
        self.last_admin_sig.borrow_mut().clear();
    }
}

/// Build the main page's static skeleton: the control rows (whose content
/// never depends on status), the peer-management rows (visibility toggled per
/// status), the members list (rows diffed per status), and the diagnostics
/// panel rows. Everything here is created exactly once.
fn build_main_page(ui: &Ui, net: &Net, window: &adw::ApplicationWindow) -> Rc<MainPage> {
    // Control group: Administration (top) → Activity log → Diagnostics →
    // Relay servers → About. Pending join requests live inside Administration
    // (opened via the Review banner or by activating the row).
    let controls = adw::PreferencesGroup::new();
    {
        let row = adw::ActionRow::builder()
            .title("Administration")
            .subtitle("Join requests, name, freeze, rotate, recovery, delete/leave")
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("emblem-system-symbolic"));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let ui2 = ui.clone();
        row.connect_activated(move |_| ui2.open("admin"));
        controls.add(&row);
    }
    {
        let row = flyout_row(
            "Activity log",
            "Administration history (last 30 days)",
            "document-open-recent-symbolic",
        );
        let net2 = net.clone();
        row.connect_activated(move |_| {
            net2.request(IpcRequest::GetAuditLog, |r| match r {
                Ok(IpcResponse::AuditLog(es)) => Some(UiMsg::AuditLog(es)),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
        });
        controls.add(&row);
    }
    {
        let row = flyout_row("Diagnostics", "Relay, connection paths, routing", "network-wired-symbolic");
        let ui2 = ui.clone();
        row.connect_activated(move |_| ui2.open("diagnostics"));
        controls.add(&row);
    }
    {
        let row = flyout_row(
            "Relay servers",
            "Use your own iroh relay instead of the public ones",
            "network-server-symbolic",
        );
        let net2 = net.clone();
        row.connect_activated(move |_| open_relays(&net2));
        controls.add(&row);
    }
    {
        // About opens the standard dialog (not a flyout) — no chevron suffix.
        let row = adw::ActionRow::builder()
            .title("About Nullgate")
            .subtitle(format!("Version {}", env!("CARGO_PKG_VERSION")))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("help-about-symbolic"));
        let window2 = window.clone();
        row.connect_activated(move |_| show_about(&window2));
        controls.add(&row);
    }

    // Peer management — Controllers and the originator only; Peers don't see
    // it. Built unconditionally, visibility toggled by `apply_status`.
    let pm_group = adw::PreferencesGroup::builder().title("Peer management").build();
    {
        let row = info_row(
            "Show join ticket (Peer level)",
            "Invite a device as a Peer",
            "send-to-symbolic",
            "Peers can use the network and view the activity log, but can't \
             approve devices or view join tickets.",
        );
        let net2 = net.clone();
        row.connect_activated(move |_| {
            net2.request(IpcRequest::GetTicket, |r| match r {
                Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
        });
        pm_group.add(&row);
    }
    let pm_controller_row = info_row(
        "Show join ticket (Controller level)",
        "Invite a device as a Controller (single-use)",
        "send-to-symbolic",
        "Controllers can add and remove Peers, but can't view the originator \
         key, rotate the secret, or delete the network.",
    );
    {
        let net2 = net.clone();
        pm_controller_row.connect_activated(move |_| {
            net2.request(IpcRequest::GetControllerTicket, |r| match r {
                Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
        });
        pm_group.add(&pm_controller_row);
    }

    // Members: our own sortable ListBox inside the PreferencesGroup (added as a
    // non-row child it renders below the header, and `boxed-list` matches the
    // group's own list styling). A plain group can't reorder rows in place —
    // and re-sorting a ListBox MOVES rows without destroying them, which is
    // exactly what lets an in-flight click survive an online/offline reorder.
    let members_group = adw::PreferencesGroup::builder().title("Members").build();
    let members_list = gtk::ListBox::new();
    members_list.set_selection_mode(gtk::SelectionMode::None);
    members_list.add_css_class("boxed-list");
    let order: Rc<RefCell<HashMap<String, u32>>> = Default::default();
    {
        let order = order.clone();
        members_list.set_sort_func(move |a, b| {
            let o = order.borrow();
            let ka = o.get(a.widget_name().as_str()).copied().unwrap_or(u32::MAX);
            let kb = o.get(b.widget_name().as_str()).copied().unwrap_or(u32::MAX);
            ka.cmp(&kb).into()
        });
    }
    members_group.add(&members_list);

    // Diagnostics flyout rows (the panel's box lives on `Ui`).
    let diag = {
        let g = adw::PreferencesGroup::new();
        let home_relay = property_row("Home relay", "—");
        let connections = property_row("Connections", "—");
        let routing = property_row("Routing (TUN)", "—");
        g.add(&home_relay);
        g.add(&connections);
        g.add(&routing);
        ui.diag_box.append(&g);
        DiagPanel { home_relay, connections, routing }
    };

    Rc::new(MainPage {
        controls,
        pm_group,
        pm_controller_row,
        members_group,
        members_list,
        rows: RefCell::new(HashMap::new()),
        order,
        diag,
        last_admin_sig: RefCell::new(String::new()),
    })
}

/// Apply a status snapshot to the persistent page, updating widgets **in
/// place** — nothing on the main page is destroyed, so clicks and keyboard
/// focus survive any status push. The admin flyout is the one remaining
/// rebuild, gated on [`admin_signature`]; the focus save/restore wrapper stays
/// as the safety net for it (and per the CLAUDE.md keyboard-nav gotcha).
fn apply_status(
    ui: &Ui,
    page: &Rc<MainPage>,
    s: &NetworkStatus,
    net: &Net,
    window: &adw::ApplicationWindow,
    pending: &Rc<RefCell<Vec<PendingJoin>>>,
    state: &Rc<RefCell<Option<NetworkStatus>>>,
) {
    let focused = focused_row_title(window);

    // Re-attach the skeleton if a placeholder page (no network / daemon down /
    // version mismatch) had replaced it; the Rc keeps the widgets alive while
    // they're detached.
    if page.controls.parent().is_none() {
        clear_box(&ui.main_box);
        ui.main_box.append(&page.controls);
        ui.main_box.append(&page.pm_group);
        ui.main_box.append(&page.members_group);
    }

    // Full-width banners are ToolbarView top bars (outside main_box), updated
    // in place. Service state first:
    set_service_banner(
        ui,
        if !s.online {
            ServiceBanner::Offline
        } else if !s.routing {
            ServiceBanner::RoutingOff
        } else {
            ServiceBanner::Hidden
        },
    );
    // Pending join requests → a Review banner (only approvers see it); Review
    // opens the admin flyout's emoji-SAS approval screen.
    let n_pending = pending.borrow().len();
    if s.self_role != "peer" && n_pending > 0 {
        ui.join_banner.set_title(&join_banner_text(n_pending));
        ui.join_banner.set_button_label(Some("Review"));
        ui.join_banner.set_revealed(true);
    } else {
        ui.join_banner.set_revealed(false);
    }

    // Role-dependent visibility (rows built once).
    page.pm_group.set_visible(s.self_role != "peer");
    page.pm_controller_row.set_visible(s.is_originator);

    // Members: diff against the live rows — update in place, add new, remove
    // departed, then re-sort (rows move, none are destroyed).
    *page.order.borrow_mut() = member_order(&s.members);
    {
        let mut rows = page.rows.borrow_mut();
        let present: std::collections::HashSet<&str> =
            s.members.iter().map(|m| m.node_id.as_str()).collect();
        let list = page.members_list.clone();
        rows.retain(|id, r| {
            let keep = present.contains(id.as_str());
            if !keep {
                list.remove(&r.row);
            }
            keep
        });
        for m in &s.members {
            match rows.get(&m.node_id) {
                Some(r) => {
                    r.row.set_title(&member_row_title(m));
                    r.row.set_subtitle(&member_row_subtitle(m));
                    apply_dot_style(&r.dot, m.online, m.last_seen, m.access_disabled, m.hidden);
                }
                None => {
                    let dot = status_dot(m.online, m.last_seen, m.access_disabled, m.hidden);
                    let row = adw::ActionRow::builder()
                        .title(member_row_title(m))
                        .subtitle(member_row_subtitle(m))
                        .activatable(true)
                        .build();
                    // The sort func's key for this row.
                    row.set_widget_name(&m.node_id);
                    row.add_prefix(&dot);
                    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                    // Connected ONCE, looking the member up at click time so
                    // the flyout always opens on fresh data (the old
                    // per-render handler captured a clone that went stale).
                    let node_id = m.node_id.clone();
                    let ui2 = ui.clone();
                    let net2 = net.clone();
                    let window2 = window.clone();
                    let state2 = state.clone();
                    row.connect_activated(move |_| {
                        let st = state2.borrow();
                        let Some(st) = st.as_ref() else { return };
                        if let Some(m) = st.members.iter().find(|m| m.node_id == node_id) {
                            fill_member(&ui2, m, &st.self_role, st.is_originator, &net2, &window2);
                        }
                    });
                    page.members_list.append(&row);
                    rows.insert(m.node_id.clone(), MemberRowUi { row, dot });
                }
            }
        }
    }
    page.members_list.invalidate_sort();
    let others = s.members.iter().filter(|m| !m.is_self).count();
    page.members_group
        .set_description(Some(&format!("{} device(s) total", others + 1)));

    // Admin flyout: rebuild-style, but only when data IT displays changed —
    // telemetry churn can no longer recreate Approve/Deny under the cursor.
    let sig = admin_signature(s, &pending.borrow());
    if *page.last_admin_sig.borrow() != sig {
        *page.last_admin_sig.borrow_mut() = sig;
        render_admin(&ui.admin_box, s, net, window, pending);
    }

    update_diag(&page.diag, s);

    if let Some(title) = focused {
        focus_row_by_title(window, &title);
    }
}

/// The title of the `adw::ActionRow` that currently holds keyboard focus, if any.
/// Used to put focus back after a rebuild. Returns `None` when focus is in a text
/// field (we must never yank an active caret out of a name/notes entry) or isn't on
/// a row at all.
fn focused_row_title(window: &adw::ApplicationWindow) -> Option<String> {
    let mut w = adw::prelude::GtkWindowExt::focus(window)?;
    if w.is::<gtk::Text>() || w.is::<gtk::TextView>() {
        return None;
    }
    loop {
        if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
            return Some(row.title().to_string());
        }
        w = w.parent()?;
    }
}

/// Re-grab keyboard focus on the first `adw::ActionRow` under `root` whose title
/// matches `title`. No-op if none matches (e.g. the row's member left the network).
fn focus_row_by_title(root: &impl IsA<gtk::Widget>, title: &str) -> bool {
    let mut child = root.as_ref().first_child();
    while let Some(w) = child {
        if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
            if row.title() == title {
                row.grab_focus();
                return true;
            }
        }
        if focus_row_by_title(&w, title) {
            return true;
        }
        child = w.next_sibling();
    }
    false
}

/// Refresh the diagnostics flyout's persistent rows in place.
fn update_diag(d: &DiagPanel, s: &NetworkStatus) {
    d.home_relay
        .set_subtitle(&s.home_relay.clone().unwrap_or_else(|| "—".into()));
    let direct = s.members.iter().filter(|m| !m.is_self && m.online && m.direct == Some(true)).count();
    let relayed = s.members.iter().filter(|m| !m.is_self && m.online && m.direct == Some(false)).count();
    d.connections
        .set_subtitle(&format!("{direct} direct · {relayed} via relay"));
    d.routing.set_subtitle(if s.routing {
        "on — carrying traffic"
    } else {
        "off — needs the elevated daemon"
    });
}

/// Fetch the current relay settings; the response renders + opens the flyout.
fn open_relays(net: &Net) {
    net.request(IpcRequest::GetRelays, |r| match r {
        Ok(IpcResponse::Relays(s)) => Some(UiMsg::Relays(s)),
        Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
        Err(_) => Some(UiMsg::DaemonDown),
        _ => None,
    });
}

/// Write relay settings to the daemon; on success the panel re-fetches (via
/// `RelaysSaved`) so it always shows the state the daemon actually applied.
fn set_relays(net: &Net, settings: RelaySettings) {
    net.request(IpcRequest::SetRelays { settings }, |r| match r {
        Ok(IpcResponse::Ok) => Some(UiMsg::RelaysSaved),
        Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
        Err(_) => Some(UiMsg::DaemonDown),
        _ => None,
    });
}

/// The "Relay servers" flyout: the configured custom relays (with add/remove),
/// and the policy for combining them with the public relays. Device-level —
/// available with or without a network.
fn render_relays(b: &gtk::Box, st: &RelayStatus, net: &Net, window: &adw::ApplicationWindow) {
    clear_box(b);
    let s = &st.settings;

    // A relay set on *some* devices is the one configuration that cannot work:
    // the configured ones advertise it as their home relay, and a peer that
    // doesn't have it (or its token) has no way to reach them. Say so up front —
    // this is the warning whose absence cost the network three days.
    if !s.servers.is_empty() {
        let warn = adw::Banner::builder()
            .title("Set these same relays — same URL and token — on every device")
            .revealed(true)
            .build();
        b.append(&warn);
    }

    // What the daemon actually managed to do with the last change.
    match &st.apply {
        RelayApply::Applied => {}
        RelayApply::Pending => {
            let g = adw::PreferencesGroup::new();
            g.add(&property_row("Applying…", "Saved; still reaching the running daemon"));
            b.append(&g);
        }
        RelayApply::Failed { reason } => {
            let banner = adw::Banner::builder()
                .title("Saved, but not applied — restart the Nullgate daemon")
                .revealed(true)
                .build();
            banner.set_tooltip_text(Some(reason));
            b.append(&banner);
        }
    }

    let list = adw::PreferencesGroup::new();
    {
        let row = adw::ActionRow::builder()
            .title("Add relay server")
            .activatable(true)
            .build();
        let add = icon_button("list-add-symbolic", "Add a relay server");
        {
            let net2 = net.clone();
            let window2 = window.clone();
            let cur = s.clone();
            add.connect_clicked(move |_| add_relay_dialog(&window2, &net2, &cur));
        }
        row.add_suffix(&add);
        {
            let net2 = net.clone();
            let window2 = window.clone();
            let cur = s.clone();
            row.connect_activated(move |_| add_relay_dialog(&window2, &net2, &cur));
        }
        list.add(&row);
    }
    for server in &s.servers {
        let row = adw::ActionRow::builder()
            .title(&server.url)
            .subtitle(if server.token.is_some() {
                "Access token set"
            } else {
                "No access token"
            })
            .build();

        // Test: connect to the relay with what's saved. Worth having on a relay
        // that is already configured — a relay can start refusing the token it
        // used to accept (rotated, revoked, redeployed), and the way you find out
        // is that peers quietly stop seeing this device.
        let test = icon_button("network-transmit-receive-symbolic", "Test this relay");
        {
            let net2 = net.clone();
            let server = server.clone();
            test.connect_clicked(move |btn| {
                let spinner = gtk::Spinner::new();
                spinner.start();
                btn.set_child(Some(&spinner));
                btn.set_sensitive(false);

                let btn2 = btn.clone();
                let net3 = net2.clone();
                let gated = server.token.is_some();
                probe_relay(&net2, server.clone(), move |res| {
                    btn2.set_sensitive(true);
                    btn2.set_icon_name("network-transmit-receive-symbolic");
                    match res {
                        Ok(()) if gated => net3.toast("The relay accepted this token."),
                        Ok(()) => net3.toast("The relay is reachable."),
                        Err(e) => net3.toast(e),
                    }
                });
            });
        }
        row.add_suffix(&test);

        let remove = icon_button("user-trash-symbolic", "Remove this relay");
        {
            let net2 = net.clone();
            let mut without = s.clone();
            let url = server.url.clone();
            without.servers.retain(|r| r.url != url);
            remove.connect_clicked(move |_| set_relays(&net2, without.clone()));
        }
        row.add_suffix(&remove);
        list.add(&row);
    }
    b.append(&list);

    // Policy only matters once a custom relay exists.
    if !s.servers.is_empty() {
        let policy = adw::PreferencesGroup::new();
        let combo = adw::ComboRow::builder()
            .title("Public iroh relays")
            .subtitle("Keeping them means peers without your relay can still reach you")
            .model(&gtk::StringList::new(&[
                "Keep them as a backup (recommended)",
                "Never use them (my relays only)",
            ]))
            .build();
        combo.set_selected(match s.mode {
            RelayPolicy::Preferred => 0,
            RelayPolicy::Only => 1,
        });
        {
            let net2 = net.clone();
            let cur = s.clone();
            combo.connect_selected_notify(move |c| {
                let mode = if c.selected() == 1 { RelayPolicy::Only } else { RelayPolicy::Preferred };
                if mode != cur.mode {
                    let mut next = cur.clone();
                    next.mode = mode;
                    set_relays(&net2, next);
                }
            });
        }
        policy.add(&combo);
        b.append(&policy);
    }
}

/// Connect to a relay with these credentials, from a throwaway endpoint in the
/// daemon, and report whether it accepted us. Takes seconds; never touches the
/// live endpoint. `done` runs on the GTK thread.
fn probe_relay(
    net: &Net,
    server: RelayServer,
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    let req = IpcRequest::ProbeRelay {
        url: server.url,
        token: server.token,
    };
    net.request_local(req, move |r| {
        done(match r {
            Ok(IpcResponse::Ok) => Ok(()),
            Ok(IpcResponse::Err(e)) => Err(e),
            Ok(_) => Err("Unexpected response from the daemon.".into()),
            Err(_) => Err("Can't reach the Nullgate daemon.".into()),
        })
    });
}

/// Dialog collecting a relay URL + optional access token, appended to `cur`.
///
/// The relay is tested before it is saved, because a relay this device can't use
/// is not a mild misconfiguration: the device homes on it, peers that can't use
/// it lose their relay path here, and — hole-punching being relay-coordinated —
/// usually the direct path too. That partition is silent, and a typo in a token
/// is enough to cause it.
///
/// So the dialog stays up for the test — progress and any failure are shown
/// *inside* it, with the text still in the fields — and only closes once the
/// relay has accepted us. That is why this is a hand-built `adw::Window` and not
/// an `adw::MessageDialog`: a MessageDialog closes itself on any response, which
/// would leave the check reporting into a dialog that no longer exists.
///
/// **Add anyway** appears once a test has failed: a wrong token and a relay
/// that is merely down are indistinguishable from here (both are "never came
/// online"), and someone configuring their devices before the relay is up has
/// done nothing wrong.
fn add_relay_dialog(window: &adw::ApplicationWindow, net: &Net, cur: &RelaySettings) {
    let url = gtk::Entry::builder()
        .placeholder_text("https://relay.example.com:8443")
        .build();
    let token = gtk::Entry::builder()
        .placeholder_text("Access token (optional)")
        .build();

    let body = gtk::Label::new(Some(
        "The address of your own iroh relay. If it requires an access token, paste it below. \
         Nullgate connects to the relay to check it before saving.",
    ));
    body.set_wrap(true);
    body.set_justify(gtk::Justification::Center);
    body.add_css_class("dim-label");

    // One status line, used for both the wait and the reason it failed.
    let spinner = gtk::Spinner::new();
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.append(&spinner);
    status_row.append(&status);
    status_row.set_visible(false);

    let cancel = gtk::Button::with_label("Cancel");
    let force = gtk::Button::with_label("Add anyway");
    force.add_css_class("destructive-action");
    force.set_visible(false);
    let add = gtk::Button::with_label("Add");
    add.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&force);
    buttons.append(&add);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.append(&body);
    content.append(&url);
    content.append(&token);
    content.append(&status_row);
    content.append(&buttons);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&content));
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Add a relay server")
        .default_width(400)
        .content(&view)
        .build();

    let save = {
        let net = net.clone();
        let cur = cur.clone();
        let dialog = dialog.clone();
        move |server: RelayServer| {
            let mut next = cur.clone();
            next.servers.retain(|r| r.url != server.url);
            next.servers.push(server);
            set_relays(&net, next);
            dialog.close();
        }
    };

    // What the user typed, or nothing if there's no URL to test.
    let entered = {
        let url = url.clone();
        let token = token.clone();
        move || {
            let url = url.text().trim().to_string();
            if url.is_empty() {
                return None;
            }
            let token = token.text().trim().to_string();
            Some(RelayServer {
                url,
                token: if token.is_empty() { None } else { Some(token) },
            })
        }
    };

    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| dialog.close()
    });
    force.connect_clicked({
        let save = save.clone();
        let entered = entered.clone();
        move |_| {
            if let Some(server) = entered() {
                save(server);
            }
        }
    });

    let start_test = {
        let net = net.clone();
        let widgets = (
            url.clone(),
            token.clone(),
            add.clone(),
            force.clone(),
            cancel.clone(),
            spinner.clone(),
            status.clone(),
            status_row.clone(),
        );
        move || {
            let (url, token, add, force, cancel, spinner, status, status_row) = widgets.clone();
            let Some(server) = entered() else { return };

            // Locked down for the duration: the fields are what is being tested,
            // and a second Add would race the first.
            for w in [url.upcast_ref::<gtk::Widget>(), token.upcast_ref()] {
                w.set_sensitive(false);
            }
            add.set_sensitive(false);
            force.set_visible(false);
            cancel.set_sensitive(false);
            status.remove_css_class("error");
            status.set_text(&format!("Testing {}…", server.url));
            spinner.start();
            spinner.set_visible(true);
            status_row.set_visible(true);

            let save = save.clone();
            let probed = server.clone();
            probe_relay(&net, server, move |res| {
                for w in [url.upcast_ref::<gtk::Widget>(), token.upcast_ref()] {
                    w.set_sensitive(true);
                }
                add.set_sensitive(true);
                cancel.set_sensitive(true);
                spinner.stop();
                spinner.set_visible(false);
                match res {
                    Ok(()) => {
                        status.set_text("The relay accepted this device.");
                        save(probed);
                    }
                    Err(e) => {
                        status.add_css_class("error");
                        status.set_text(&e);
                        // Now that they've seen why, let them overrule it.
                        force.set_visible(true);
                        add.set_label("Try again");
                    }
                }
            });
        }
    };

    add.connect_clicked({
        let start_test = start_test.clone();
        move |_| start_test()
    });
    for entry in [&url, &token] {
        entry.connect_activate({
            let start_test = start_test.clone();
            move |_| start_test()
        });
    }

    dialog.present();
    url.grab_focus();
}

fn render_admin(
    b: &gtk::Box,
    s: &NetworkStatus,
    net: &Net,
    window: &adw::ApplicationWindow,
    pending: &Rc<RefCell<Vec<PendingJoin>>>,
) {
    clear_box(b);

    // Pending join requests — Controllers and the originator only.
    if s.self_role != "peer" {
        let plist = pending.borrow();
        if !plist.is_empty() {
            let area = gtk::Box::new(gtk::Orientation::Vertical, 8);
            area.add_css_class("attention-bg");
            let title = gtk::Label::new(Some("Join requests"));
            title.add_css_class("title-4");
            title.set_halign(gtk::Align::Center);
            area.append(&title);
            let hint = gtk::Label::new(Some(
                "Approve only if the emoji code matches the joining device's screen.",
            ));
            hint.add_css_class("dim-label");
            hint.set_wrap(true);
            hint.set_justify(gtk::Justification::Center);
            hint.set_halign(gtk::Align::Center);
            area.append(&hint);
            for (i, req) in plist.iter().enumerate() {
                if i > 0 {
                    area.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                }
                area.append(&request_card(req, net, pending));
            }
            b.append(&area);
        }
    }

    // Network name (rename here, not on the main screen) — Controllers and the
    // originator only.
    if s.self_role != "peer" {
        let name_group = adw::PreferencesGroup::new();
        let name_row = adw::ActionRow::builder()
            .title("Network name")
            .subtitle(&s.name)
            .build();
        let edit = icon_button("document-edit-symbolic", "Rename the network");
        let net2 = net.clone();
        let window2 = window.clone();
        let cur = s.name.clone();
        edit.connect_clicked(move |_| rename_dialog(&window2, &net2, &cur));
        name_row.add_suffix(&edit);
        name_group.add(&name_row);
        b.append(&name_group);
    }

    let g = adw::PreferencesGroup::new();
    if s.is_originator {
        let freeze = gtk::Switch::builder().active(s.frozen).valign(gtk::Align::Center).build();
        let net2 = net.clone();
        freeze.connect_state_set(move |_, state| {
            net2.request(IpcRequest::SetFrozen { frozen: state }, move |r| match r {
                Ok(IpcResponse::Ok) => Some(UiMsg::Toast(
                    if state { "Membership frozen" } else { "Membership unfrozen" }.into(),
                )),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
            glib::Propagation::Proceed
        });
        let frow = adw::ActionRow::builder()
            .title("Freeze membership")
            .subtitle("No new devices can join while frozen")
            .build();
        frow.add_suffix(&freeze);
        g.add(&frow);

        let rotate = action_row("Rotate secret (re-key)", "Removes everyone; mints a fresh ticket");
        let net2 = net.clone();
        let window2 = window.clone();
        rotate.connect_activated(move |_| confirm_rotate(&window2, &net2));
        g.add(&rotate);

        let backup = action_row("Back up originator key", "Save a recovery code to restore admin elsewhere");
        let net2 = net.clone();
        backup.connect_activated(move |_| {
            net2.request(IpcRequest::ExportOriginatorKey, |r| match r {
                Ok(IpcResponse::Recovery(code)) => Some(UiMsg::Recovery(code)),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
        });
        g.add(&backup);

        // Peer-ticket single-use: toggling mints a new code, invalidating the old.
        let single = gtk::Switch::builder()
            .active(s.peer_ticket_single_use)
            .valign(gtk::Align::Center)
            .build();
        let net2 = net.clone();
        single.connect_state_set(move |_, state| {
            net2.request(IpcRequest::SetPeerTicketSingleUse { on: state }, move |r| match r {
                Ok(IpcResponse::Ok) => Some(UiMsg::Toast(
                    if state {
                        "Peer tickets are now single-use (new code issued)"
                    } else {
                        "Peer tickets are now reusable (new code issued)"
                    }
                    .into(),
                )),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
            glib::Propagation::Proceed
        });
        let srow = adw::ActionRow::builder()
            .title("Single-use Peer tickets")
            .subtitle("Each Peer ticket admits one device; toggling issues a fresh code")
            .build();
        srow.add_suffix(&single);
        g.add(&srow);
    } else {
        let restore = action_row("Restore originator access…", "Paste a recovery code to gain admin powers");
        let net2 = net.clone();
        let window2 = window.clone();
        restore.connect_activated(move |_| import_originator_dialog(&window2, &net2));
        g.add(&restore);
    }
    b.append(&g);

    let danger = adw::PreferencesGroup::new();
    let row = action_row(
        if s.is_originator { "Delete network" } else { "Leave network" },
        if s.is_originator { "Dissolve the network for everyone" } else { "Leave on this device only" },
    );
    row.add_css_class("error");
    let net2 = net.clone();
    let window2 = window.clone();
    let is_orig = s.is_originator;
    row.connect_activated(move |_| confirm_destroy(&window2, &net2, is_orig));
    danger.add(&row);
    b.append(&danger);
}

/// One pending-join card: who + big emoji code + Approve/Deny.
fn request_card(req: &PendingJoin, net: &Net, pending: &Rc<RefCell<Vec<PendingJoin>>>) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.set_margin_top(8);
    card.set_margin_bottom(8);

    let who = gtk::Label::new(Some(&format!("“{}” wants to join", req.hostname)));
    who.add_css_class("title-3");
    who.set_halign(gtk::Align::Center);
    who.set_wrap(true);
    card.append(&who);

    // Big emojis, matching the joiner's "Verify this code" page.
    card.append(&sas_label(&req.sas));

    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btns.set_halign(gtk::Align::Center);
    let deny = gtk::Button::with_label("Deny");
    deny.add_css_class("pill");
    let approve = gtk::Button::with_label("Approve");
    approve.add_css_class("pill");
    approve.add_css_class("suggested-action");

    let net_a = net.clone();
    let pending_a = pending.clone();
    let id_a = req.node_id.clone();
    approve.connect_clicked(move |_| {
        pending_a.borrow_mut().retain(|p| p.node_id != id_a);
        net_a.request(IpcRequest::ApproveJoin { node_id: id_a.clone() }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Approved".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
        net_a.refresh();
    });
    let net_d = net.clone();
    let pending_d = pending.clone();
    let id_d = req.node_id.clone();
    deny.connect_clicked(move |_| {
        pending_d.borrow_mut().retain(|p| p.node_id != id_d);
        net_d.request(IpcRequest::DenyJoin { node_id: id_d.clone() }, |_| None);
        net_d.toast("Join denied");
        net_d.refresh();
    });
    btns.append(&deny);
    btns.append(&approve);
    card.append(&btns);
    card
}

/// One-line preview of a note for the member-row subtitle ("Add a note" if empty).
fn note_preview(note: Option<&str>) -> String {
    match note {
        Some(n) if !n.trim().is_empty() => {
            let first = n.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if first.chars().count() > 40 {
                format!("{}…", first.chars().take(40).collect::<String>())
            } else {
                first.to_string()
            }
        }
        _ => "Add a note".to_string(),
    }
}

/// Fill + open the per-member detail flyout.
fn fill_member(
    ui: &Ui,
    m: &MemberView,
    self_role: &str,
    is_originator: bool,
    net: &Net,
    window: &adw::ApplicationWindow,
) {
    let display = m
        .label
        .clone()
        .or_else(|| m.hostname.clone())
        .unwrap_or_else(|| short_id(&m.node_id));
    let member_title = if m.is_self {
        format!("{display} (this device)")
    } else {
        display.clone()
    };
    ui.member_title.set_title(&member_title);

    clear_box(&ui.member_box);
    let g = adw::PreferencesGroup::new();

    // Status (top) with a colored dot (white hidden / yellow blocked / green /
    // gray / red>1wk).
    let status_text = if m.hidden {
        "Hidden from the member list".to_string()
    } else if m.access_disabled {
        if m.is_self {
            "Online · remote access disabled (this device)".to_string()
        } else {
            "Online · remote access disabled".to_string()
        }
    } else if m.online {
        if m.is_self { "Online (this device)".to_string() } else { "Online".to_string() }
    } else if m.last_seen == 0 {
        "Offline".to_string()
    } else {
        format!("Offline · last seen {}", fmt_last_seen(m.last_seen))
    };
    let status_row = property_row("Status", &status_text);
    status_row.add_prefix(&status_dot(m.online, m.last_seen, m.access_disabled, m.hidden));
    g.add(&status_row);

    // Notes — a local, free-text note about this member (never shared).
    if !m.is_self {
        // Prefer the value we edited this session for the preview, too.
        let cached = ui.notes_cache.borrow().get(&m.node_id).cloned();
        let preview = note_preview(cached.as_deref().or(m.note.as_deref()));
        let notes_row = action_row("Notes", &preview);
        notes_row.add_prefix(&gtk::Image::from_icon_name("document-edit-symbolic"));
        // Remember this row so saving can refresh its preview without a rebuild.
        *ui.notes_row.borrow_mut() = Some(notes_row.clone());
        let ui2 = ui.clone();
        let m2 = m.clone();
        notes_row.connect_activated(move |_| {
            *ui2.notes_target.borrow_mut() = Some(m2.node_id.clone());
            // Prefer what we edited this session (instant); else the value from
            // the last status (loaded from disk).
            let text = ui2
                .notes_cache
                .borrow()
                .get(&m2.node_id)
                .cloned()
                .unwrap_or_else(|| m2.note.clone().unwrap_or_default());
            ui2.notes_view.buffer().set_text(&text);
            ui2.open("notes");
        });
        g.add(&notes_row);
    }

    // Friendly name — set by THIS client for another member (local; not shared).
    if !m.is_self {
        let name_row = adw::ActionRow::builder()
            .title("Friendly name")
            .subtitle(m.label.clone().unwrap_or_else(|| "(none)".into()))
            .build();
        let edit = icon_button("document-edit-symbolic", "Set a local nickname for this member");
        let window2 = window.clone();
        let net2 = net.clone();
        let ui2 = ui.clone();
        let m2 = m.clone();
        let self_role2 = self_role.to_string();
        edit.connect_clicked(move |_| {
            set_nickname_dialog(&window2, &net2, &ui2, &m2, &self_role2, is_originator)
        });
        name_row.add_suffix(&edit);
        g.add(&name_row);
    }

    g.add(&property_row("Hostname", &m.hostname.clone().unwrap_or_else(|| "—".into())));

    if let Some(ip) = &m.virtual_ip {
        let row = property_row("Virtual IP", ip);
        let copy = icon_button("edit-copy-symbolic", "Copy virtual IP");
        let ip = ip.clone();
        let win = window.clone();
        let net2 = net.clone();
        copy.connect_clicked(move |_| {
            win.clipboard().set_text(&ip);
            net2.toast("Virtual IP copied");
        });
        row.add_suffix(&copy);
        g.add(&row);
    }
    g.add(&property_row("Local IP", m.local_ip.as_deref().unwrap_or("—")));
    g.add(&property_row("Public IP", m.public_ip.as_deref().unwrap_or("—")));

    // Location: the required attribution link sits inline next to the header; a
    // help icon after the value carries the "approximate" explainer as a tooltip.
    let loc_row = adw::ActionRow::builder()
        .title(
            "Location   <a href=\"https://db-ip.com/\">\
             <span size=\"small\">IP Geolocation by DB-IP</span></a>",
        )
        .subtitle(m.location.as_deref().unwrap_or("—"))
        .build();
    loc_row.set_use_markup(true);
    loc_row.set_subtitle_selectable(true);
    loc_row.add_css_class("property");
    let help = gtk::Image::from_icon_name("help-about-symbolic");
    help.set_tooltip_text(Some("Approximate, based on the public IP."));
    help.set_valign(gtk::Align::Center);
    loc_row.add_suffix(&help);
    g.add(&loc_row);

    let id_row = property_row("Node ID", &m.node_id);
    let copy = icon_button("edit-copy-symbolic", "Copy node ID");
    let nid = m.node_id.clone();
    let win = window.clone();
    let net2 = net.clone();
    copy.connect_clicked(move |_| {
        win.clipboard().set_text(&nid);
        net2.toast("Node ID copied");
    });
    id_row.add_suffix(&copy);
    g.add(&id_row);

    // Observed address — kept, at the bottom.
    g.add(&property_row(
        "Observed address",
        m.observed_addr.as_deref().unwrap_or("—"),
    ));
    ui.member_box.append(&g);

    // This device: Controllers and the originator get the two access switches.
    if m.is_self && self_role != "peer" {
        let dev = adw::PreferencesGroup::builder().title("This device").build();

        // The block is on whenever access is disabled *or* the device is hidden
        // (hiding implies the block). While hidden, the switch is forced on and
        // locked — its enabling is implicit.
        let block_sw = gtk::Switch::builder()
            .active(m.access_disabled || m.hidden)
            .valign(gtk::Align::Center)
            .build();
        block_sw.set_sensitive(!m.hidden);
        {
            let net2 = net.clone();
            block_sw.connect_state_set(move |_, state| {
                net2.request(
                    IpcRequest::SetRemoteAccessDisabled { disabled: state },
                    |r| match r {
                        Ok(IpcResponse::Ok) => Some(UiMsg::Refresh),
                        Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                        _ => None,
                    },
                );
                glib::Propagation::Proceed
            });
        }
        let block_row = adw::ActionRow::builder()
            .title("Disable remote access")
            .subtitle("Block others from reaching this device — you can still reach them")
            .build();
        block_row.add_suffix(&block_sw);
        dev.add(&block_row);

        let hide_sw = gtk::Switch::builder()
            .active(m.hidden)
            .valign(gtk::Align::Center)
            .build();
        {
            let net2 = net.clone();
            let block_sw = block_sw.clone();
            hide_sw.connect_state_set(move |_, state| {
                // Hiding implies the block: turn it on and lock the switch. Releasing
                // hide just unlocks it again (the block stays until manually cleared).
                if state {
                    block_sw.set_active(true);
                }
                block_sw.set_sensitive(!state);
                net2.request(IpcRequest::SetHidden { hidden: state }, |r| match r {
                    Ok(IpcResponse::Ok) => Some(UiMsg::Refresh),
                    Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                    _ => None,
                });
                glib::Propagation::Proceed
            });
        }
        let hide_row = adw::ActionRow::builder()
            .title("Hide this device from member list")
            .subtitle("Also disables remote access; only originators still see it")
            .build();
        hide_row.add_suffix(&hide_sw);
        dev.add(&hide_row);

        ui.member_box.append(&dev);
    }

    // Remove: the originator can remove anyone; a Controller can remove a Peer.
    let show_remove = !m.is_self
        && (is_originator || (self_role == "controller" && m.role == "peer"));
    if show_remove {
        let danger = adw::PreferencesGroup::new();
        let kick = action_row("Remove from network", "Kicks this device and drops its connection");
        kick.add_css_class("error");
        let net2 = net.clone();
        let window2 = window.clone();
        let ui2 = ui.clone();
        let id = m.node_id.clone();
        let name = display.clone();
        kick.connect_activated(move |_| confirm_kick(&window2, &net2, &ui2, &id, &name));
        danger.add(&kick);
        ui.member_box.append(&danger);
    }

    ui.open("member");
}

/// Fill + open the join-ticket flyout (QR + key + copy).
fn fill_ticket(ui: &Ui, ticket: &str, net: &Net, window: &adw::ApplicationWindow) {
    clear_box(&ui.ticket_box);
    if let Some(pic) = qr_picture(ticket) {
        ui.ticket_box.append(&pic);
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder().text(ticket).editable(false).hexpand(true).build();
    let copy = icon_button("edit-copy-symbolic", "Copy ticket");
    let ticket_owned = ticket.to_string();
    let win = window.clone();
    let net2 = net.clone();
    copy.connect_clicked(move |_| {
        win.clipboard().set_text(&ticket_owned);
        net2.toast("Ticket copied");
    });
    row.append(&entry);
    row.append(&copy);
    ui.ticket_box.append(&row);

    let hint = gtk::Label::new(Some(
        "Scan the QR from the other device, or copy the ticket and paste it into Join.",
    ));
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    ui.ticket_box.append(&hint);

    ui.open("ticket");
}

/// Fill + open the activity-log flyout (administration history, newest first).
fn fill_audit(ui: &Ui, entries: &[AuditEntry]) {
    clear_box(&ui.audit_box);
    let g = adw::PreferencesGroup::new();
    if entries.is_empty() {
        g.add(&property_row(
            "No activity",
            "Nothing has been recorded in the last 30 days.",
        ));
    } else {
        for e in entries {
            let who = e
                .actor_name
                .clone()
                .unwrap_or_else(|| short_id(&e.actor_node_id));
            let row = adw::ActionRow::builder()
                .title(&e.action)
                .subtitle(format!("{} · {}", who, fmt_last_seen(e.ts)))
                .build();
            row.add_css_class("property");
            g.add(&row);
        }
    }
    ui.audit_box.append(&g);
    ui.open("audit");
}

// ---------------------------------------------------------------------------
// Small widget helpers
// ---------------------------------------------------------------------------

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    b.add_css_class("flat");
    b
}

/// A read-only "title / value" row (value selectable for copy).
fn property_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(value).build();
    row.add_css_class("property");
    row.set_subtitle_selectable(true);
    row
}

/// An activatable row with a trailing chevron (drills into a flyout / action).
fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

/// An activatable row with a leading icon + trailing chevron (opens a flyout).
fn flyout_row(title: &str, subtitle: &str, icon: &str) -> adw::ActionRow {
    let row = action_row(title, subtitle);
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Like [`flyout_row`] but with a hover-over info icon (tooltip) before the
/// chevron — used to explain join-ticket tiers.
fn info_row(title: &str, subtitle: &str, icon: &str, tip: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    let help = gtk::Image::from_icon_name("help-about-symbolic");
    help.set_tooltip_text(Some(tip));
    help.set_valign(gtk::Align::Center);
    row.add_suffix(&help);
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

fn rename_dialog(window: &adw::ApplicationWindow, net: &Net, current: &str) {
    let entry = gtk::Entry::builder().text(current).build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Rename network")
        .body("The name is shared with all members of this network.")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "save" {
            return;
        }
        let name = entry.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        net.request(IpcRequest::SetNetworkName { name }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Network renamed".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
}

fn confirm_kick(window: &adw::ApplicationWindow, net: &Net, ui: &Ui, node_id: &str, name: &str) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading(format!("Remove “{name}”?"))
        .body("This device is kicked from the network and its connection is dropped. You can re-invite it later with the join ticket.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    let net = net.clone();
    let ui = ui.clone();
    let id = node_id.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "remove" {
            return;
        }
        net.request(IpcRequest::RemoveMember { node_id: id.clone() }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Member removed".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
        ui.close_flyout(); // back out of the now-stale member detail
    });
    dialog.present();
}

fn confirm_rotate(window: &adw::ApplicationWindow, net: &Net) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Rotate the network secret?")
        .body(
            "Every member is removed and the network is re-keyed with a fresh secret. \
             Anyone holding the old ticket — including a device that was offline — is locked \
             out. You'll get a NEW ticket to re-invite the devices you want to keep.",
        )
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rotate", "Rotate");
    dialog.set_response_appearance("rotate", adw::ResponseAppearance::Destructive);
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "rotate" {
            return;
        }
        net.request(IpcRequest::RotateNetwork, |r| match r {
            Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
}

fn confirm_destroy(window: &adw::ApplicationWindow, net: &Net, is_originator: bool) {
    let (heading, body, label, req) = if is_originator {
        (
            "Delete this network?",
            "This removes every member and dissolves the pool — nobody will be able to reach \
             each other over it. This can't be undone.",
            "Delete",
            IpcRequest::DeleteNetwork,
        )
    } else {
        (
            "Leave this network?",
            "This device will leave the network. Other members are unaffected.",
            "Leave",
            IpcRequest::LeaveNetwork,
        )
    };
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("go", label);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "go" {
            return;
        }
        net.request(req.clone(), |r| match r {
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
}

fn create_dialog(window: &adw::ApplicationWindow, net: &Net) {
    let entry = gtk::Entry::builder().text("home").build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Create a network")
        .body("Name your private network. You'll become its originator.")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "create" {
            return;
        }
        let name = entry.text().to_string();
        let name = if name.trim().is_empty() { "home".into() } else { name };
        net.request(IpcRequest::CreateNetwork { name }, |r| match r {
            Ok(IpcResponse::Ticket(_)) => Some(UiMsg::Toast("Network created".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("create failed: {e}"))),
            Err(_) => Some(UiMsg::DaemonDown),
            _ => None,
        });
    });
    dialog.present();
}

fn join_dialog(window: &adw::ApplicationWindow, net: &Net) {
    let entry = gtk::Entry::builder().placeholder_text("ng1...").build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Join a network")
        .body("Paste the join ticket from a member. You'll verify an emoji code together.")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("join", "Join");
    dialog.set_response_appearance("join", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("join"));
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "join" {
            return;
        }
        let ticket = entry.text().to_string();
        if !ticket.trim().starts_with("ng1") {
            net.toast("That doesn't look like a join ticket (it should start with “ng1…”).");
            return;
        }
        let ticket = ticket.trim().to_string();
        net.request(IpcRequest::Join { ticket }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Joined!".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("join failed: {e}"))),
            Err(_) => Some(UiMsg::DaemonDown),
            _ => None,
        });
    });
    dialog.present();
}

fn set_nickname_dialog(
    window: &adw::ApplicationWindow,
    net: &Net,
    ui: &Ui,
    m: &MemberView,
    self_role: &str,
    is_originator: bool,
) {
    let self_role = self_role.to_string();
    let entry = gtk::Entry::builder()
        .text(m.label.clone().unwrap_or_default())
        .placeholder_text("Nickname (leave blank to clear)")
        .build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Set a friendly name")
        .body(
            "A nickname for this member, stored only on this device (not shared). The hostname \
             stays the shared identifier.",
        )
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    let net = net.clone();
    let ui = ui.clone();
    let m = m.clone();
    let window = window.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "save" {
            return;
        }
        let text = entry.text().to_string();
        let name = if text.trim().is_empty() { None } else { Some(text) };
        net.request(
            IpcRequest::SetNickname {
                node_id: m.node_id.clone(),
                name: name.clone(),
            },
            |r| match r {
                Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Nickname updated".into())),
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            },
        );
        // Redraw the open detail flyout immediately (optimistic — the nickname is
        // local, so it effectively never fails), and refresh the main list.
        let mut updated = m.clone();
        updated.label = name;
        fill_member(&ui, &updated, &self_role, is_originator, &net, &window);
        net.refresh();
    });
    dialog.present();
}

fn import_originator_dialog(window: &adw::ApplicationWindow, net: &Net) {
    let entry = gtk::Entry::builder().placeholder_text("ngkey1...").build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Restore originator access")
        .body("Paste the originator recovery code for THIS network to gain admin powers here.")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("import", "Restore");
    dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("import"));
    let net = net.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "import" {
            return;
        }
        let code = entry.text().to_string();
        net.request(IpcRequest::ImportOriginatorKey { code }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Originator access restored".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
}

fn show_recovery(window: &adw::ApplicationWindow, net: &Net, code: &str) {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if let Some(pic) = qr_picture(code) {
        vbox.append(&pic);
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder().text(code).editable(false).hexpand(true).build();
    let copy = icon_button("edit-copy-symbolic", "Copy recovery code");
    let code_owned = code.to_string();
    let win = window.clone();
    let net2 = net.clone();
    copy.connect_clicked(move |_| {
        win.clipboard().set_text(&code_owned);
        net2.toast("Recovery code copied");
    });
    row.append(&entry);
    row.append(&copy);
    vbox.append(&row);

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Originator recovery code")
        .body(
            "Store this somewhere safe (password manager / offline). Anyone who has it can \
             administer this network. Use it to restore originator access on a replacement device.",
        )
        .extra_child(&vbox)
        .build();
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.present();
}

fn show_join_sas(window: &adw::ApplicationWindow, sas: &[String]) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Verify this code")
        .body("Confirm these emojis match on the device that's approving you. Waiting for approval…")
        .extra_child(&sas_label(sas))
        .build();
    dialog.add_response("ok", "OK");
    dialog.present();
}

fn show_about(window: &adw::ApplicationWindow) {
    // No `comments` → no "Details" page. `website`/`issue_url` add the "Website"
    // and "Report an Issue" links on the main page. (These repo URLs become live
    // once the project is public.)
    let about = adw::AboutWindow::builder()
        .transient_for(window)
        .application_name("Nullgate")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("kznjk")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/steeb-k/nullgate")
        .issue_url("https://github.com/steeb-k/nullgate/issues")
        .build();
    about.present();
}

/// The emoji SAS, laid out in fixed, symmetric rows so it looks identical on the
/// joiner's "Verify this code" dialog and the originator's requests flyout
/// (relying on text wrapping made them differ by container width). The usual
/// 7-emoji code is arranged 2 / 3 / 2; other lengths fall back to rows of ≤3.
fn sas_label(sas: &[String]) -> gtk::Box {
    let pattern: Vec<usize> = if sas.len() == 7 {
        vec![2, 3, 2]
    } else {
        let mut p = Vec::new();
        let mut left = sas.len();
        while left > 0 {
            let take = left.min(3);
            p.push(take);
            left -= take;
        }
        p
    };

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_halign(gtk::Align::Center);
    let mut idx = 0;
    for count in pattern {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_halign(gtk::Align::Center);
        for _ in 0..count {
            let Some(e) = sas.get(idx) else { break };
            let lbl = gtk::Label::new(None);
            // Pin an emoji-capable font so glyphs like ✂️ render in color instead of
            // tofu: the window CSS pins "Segoe UI Variable Text", which lacks many
            // emoji and (with that override in place) doesn't reliably fall back.
            lbl.set_markup(&format!(
                "<span size='350%' font_family='Segoe UI Emoji,Noto Color Emoji,Apple Color Emoji,sans-serif'>{}</span>",
                glib::markup_escape_text(e)
            ));
            row.append(&lbl);
            idx += 1;
        }
        vbox.append(&row);
    }
    vbox
}

/// Render the ticket/recovery string as a fixed-size QR image (~240px).
fn qr_picture(data: &str) -> Option<gtk::Picture> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let quiet = 4usize;
    let modules = w + 2 * quiet;
    let scale = (240 / modules).max(2);
    let dim = modules * scale;
    let mut buf = vec![255u8; dim * dim * 3]; // white RGB
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let py = (y + quiet) * scale + dy;
                        let px = (x + quiet) * scale + dx;
                        let idx = (py * dim + px) * 3;
                        buf[idx] = 0;
                        buf[idx + 1] = 0;
                        buf[idx + 2] = 0;
                    }
                }
            }
        }
    }
    let bytes = glib::Bytes::from_owned(buf);
    let tex = gtk::gdk::MemoryTexture::new(
        dim as i32,
        dim as i32,
        gtk::gdk::MemoryFormat::R8g8b8,
        &bytes,
        dim * 3,
    );
    let pic = gtk::Picture::for_paintable(&tex);
    pic.set_size_request(dim as i32, dim as i32);
    pic.set_halign(gtk::Align::Center);
    Some(pic)
}

// --- helpers ---

const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A colored status dot: white hidden, yellow access-disabled, green online, gray
/// offline, red if offline > 1 week.
fn status_dot(online: bool, last_seen: u64, access_disabled: bool, hidden: bool) -> gtk::Label {
    let dot = gtk::Label::new(Some("●"));
    dot.set_valign(gtk::Align::Center);
    apply_dot_style(&dot, online, last_seen, access_disabled, hidden);
    dot
}

/// (Re)style a status dot in place — the persistent member rows restyle theirs
/// on every status instead of being recreated.
fn apply_dot_style(dot: &gtk::Label, online: bool, last_seen: u64, access_disabled: bool, hidden: bool) {
    let (class, tip) = if hidden {
        ("status-hidden", "Hidden")
    } else if access_disabled {
        ("warning", "Access disabled")
    } else if online {
        ("success", "Online")
    } else if last_seen != 0 && now_ms().saturating_sub(last_seen) > WEEK_MS {
        ("error", "Offline (over a week)")
    } else {
        ("dim-label", "Offline")
    };
    // Replaces any previous state class in one go.
    dot.set_css_classes(&["status-dot", class]);
    dot.set_tooltip_text(Some(tip));
}

fn short_id(hex: &str) -> String {
    hex.chars().take(10).collect()
}

fn fmt_last_seen(ms: u64) -> String {
    if ms == 0 {
        return "never".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = now.saturating_sub(ms) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn args(extra: &[&str]) -> Vec<String> {
        // argv[0] is always the program path; extra flags follow.
        std::iter::once("nullgate".to_string()).chain(extra.iter().map(|s| s.to_string())).collect()
    }

    /// No flags → the GUI window.
    #[test]
    fn bare_is_gui() {
        assert_eq!(parse_mode(&args(&[])), Mode::Gui);
    }

    /// `--agent` selects the headless tray agent.
    #[test]
    fn agent_flag_is_agent() {
        assert_eq!(parse_mode(&args(&["--agent"])), Mode::Agent);
    }

    /// `--version` / `-V` short-circuit to a version print regardless of other flags.
    #[test]
    fn version_wins() {
        assert_eq!(parse_mode(&args(&["--version"])), Mode::Version);
        assert_eq!(parse_mode(&args(&["-V"])), Mode::Version);
        // Version takes precedence even if --agent is also present.
        assert_eq!(parse_mode(&args(&["--agent", "--version"])), Mode::Version);
    }

    /// A stale `--minimized` from an old autostart entry is ignored (now GUI mode),
    /// so upgrading installs don't leave a window that never appears.
    #[test]
    fn legacy_minimized_is_gui() {
        assert_eq!(parse_mode(&args(&["--minimized"])), Mode::Gui);
    }

    fn member(id: &str, online: bool) -> MemberView {
        MemberView {
            node_id: id.into(),
            hostname: Some(format!("host-{id}")),
            label: None,
            note: None,
            virtual_ip: Some("10.99.0.7".into()),
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
            name: "home".into(),
            subnet: "10.99.0.0/24".into(),
            frozen: false,
            self_node_id: "self".into(),
            self_ip: Some("10.99.0.1".into()),
            is_originator: true,
            self_role: "originator".into(),
            peer_ticket_single_use: false,
            routing: true,
            online: true,
            home_relay: Some("https://relay.example".into()),
            members,
        }
    }

    /// The admin flyout only rebuilds when ITS data changes — per-tick
    /// telemetry (direct flips, IPs, last-seen, home relay) must not move
    /// this signature, or Approve/Deny buttons get recreated under the cursor.
    #[test]
    fn admin_signature_ignores_telemetry() {
        let mut s = status(vec![member("a", true)]);
        let base = admin_signature(&s, &[]);
        s.members[0].direct = Some(true);
        s.members[0].public_ip = Some("1.2.3.4".into());
        s.members[0].last_seen = 999_999;
        s.members[0].online = false;
        s.home_relay = None;
        assert_eq!(admin_signature(&s, &[]), base);
        // ...but its own inputs do move it.
        s.frozen = true;
        assert_ne!(admin_signature(&s, &[]), base);
        let with_pending = admin_signature(
            &status(vec![member("a", true)]),
            &[PendingJoin { node_id: "x".into(), hostname: "h".into(), sas: vec![] }],
        );
        assert_ne!(with_pending, base);
    }

    /// Self pins to the top only while it's a normal, visible device.
    #[test]
    fn member_order_pins_normal_self() {
        let mut me = member("me", true);
        me.is_self = true;
        let order = member_order(&[member("a", true), me.clone(), member("b", false)]);
        assert_eq!(order["me"], 0);
        // A hidden self stays in its engine-ranked spot instead.
        me.hidden = true;
        let order = member_order(&[member("a", true), me, member("b", false)]);
        assert_eq!(order["a"], 0);
    }

    #[test]
    fn member_row_text() {
        let mut m = member("abcdef1234ffff", true);
        assert_eq!(member_row_title(&m), "host-abcdef1234ffff");
        assert_eq!(member_row_subtitle(&m), "10.99.0.7");
        m.direct = Some(false);
        assert_eq!(member_row_subtitle(&m), "10.99.0.7 · relay");
        m.label = Some("Attic PC".into());
        assert_eq!(member_row_title(&m), "Attic PC");
        assert!(member_row_subtitle(&m).starts_with("host-abcdef1234ffff · "));
        m.is_self = true;
        assert_eq!(member_row_title(&m), "Attic PC (this device)");
        m.hidden = true;
        assert_eq!(member_row_subtitle(&m), "host-abcdef1234ffff · 10.99.0.7 · Hidden");
    }
}
