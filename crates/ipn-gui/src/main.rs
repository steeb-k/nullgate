//! iroh-private-network desktop GUI (GTK4 + libadwaita) — an **unprivileged IPC
//! client** to `ipn-daemon`. The daemon owns the iroh node + TUN (the only thing
//! needing elevation); this process just renders state and sends commands, so it
//! never needs admin/root.
//!
//! Threading: a Tokio runtime on a side thread does the socket IO; results and
//! pushed events arrive on the GTK main thread via an `async-channel` consumed by
//! `glib::spawn_future_local`. GTK objects are only touched on the main thread.
//!
//! Layout (SEED-style): a static "IPN" titlebar; a main page with a control group
//! (Administration, Show join ticket, Diagnostics) and a Members list at the
//! bottom (this device included). Each control row, and each member, opens a
//! slide-in **flyout** — an `adw::OverlaySplitView` sidebar that overlays the
//! content (the window stays visible behind it), so it reads as a sub-menu.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use ipn_ipc::transport::{self, read_frame, write_frame};
use ipn_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, MemberView, Message, NetworkStatus};
use tokio::runtime::Handle;

mod tray;

const APP_ID: &str = "io.github.steeb_k.IPN";

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
    Toast(String),
    /// Re-render the current status (e.g. after a pending-join change).
    Refresh,
    DaemonDown,
    VersionMismatch { daemon: u32, gui: u32 },
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

    /// Push a transient toast to the UI from the GTK thread (synchronous callers).
    fn toast(&self, msg: impl Into<String>) {
        let _ = self.tx.try_send(UiMsg::Toast(msg.into()));
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
        if let Message::Response(IpcResponse::Hello { version }) = frame.body {
            if version != ipn_ipc::PROTO_VERSION {
                let _ = tx
                    .send(UiMsg::VersionMismatch {
                        daemon: version,
                        gui: ipn_ipc::PROTO_VERSION,
                    })
                    .await;
                return Ok(());
            }
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
    directories::ProjectDirs::from("io.github", "steeb_k", "ipn")
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

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ipn {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    let start_minimized =
        args.iter().any(|a| a == "--minimized") || std::env::var_os("IPN_START_MINIMIZED").is_some();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

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

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, net.clone(), rx.clone(), start_minimized));
    let empty: [&str; 0] = [];
    app.run_with_args(&empty)
}

/// Handles to the persistent widgets, passed to the render functions.
#[derive(Clone)]
struct Ui {
    split: adw::OverlaySplitView,
    stack: gtk::Stack,
    main_box: gtk::Box,
    admin_box: gtk::Box,
    diag_box: gtk::Box,
    requests_box: gtk::Box,
    ticket_box: gtk::Box,
    member_box: gtk::Box,
    member_title: adw::WindowTitle,
}

impl Ui {
    /// Reveal a flyout by stack name (it overlays the content; window stays behind).
    fn open(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        self.split.set_show_sidebar(true);
    }
    fn close_flyout(&self) {
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

fn build_ui(
    app: &adw::Application,
    net: Net,
    rx: async_channel::Receiver<UiMsg>,
    start_minimized: bool,
) {
    load_css();

    let (win_w, win_h) = load_window_size();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("IPN")
        .default_width(win_w)
        .default_height(win_h)
        .build();

    // --- main header (static branding; carries the window controls) ---
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("IPN", "Iroh Private Network")));

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
    main_toolbar.set_content(Some(&main_scroller));

    // --- flyout: an overlay sidebar (kept collapsed → always overlays the content
    // with a scrim + slide, the window visible behind it). It's the TOP-LEVEL
    // widget so the flyout spans the full window height — over the header too,
    // like SEED — rather than sitting below it. A stack swaps which panel fills it. ---
    let split = adw::OverlaySplitView::new();
    split.set_collapsed(true);
    split.set_sidebar_position(gtk::PackType::End);
    split.set_show_sidebar(false);
    split.set_min_sidebar_width(300.0);
    split.set_max_sidebar_width(460.0);
    split.set_sidebar_width_fraction(0.72);
    split.set_content(Some(&main_toolbar));

    let admin_box = padded_box();
    let diag_box = padded_box();
    let requests_box = padded_box();
    let ticket_box = padded_box();
    let member_box = padded_box();

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
            let split = split.clone();
            back.connect_clicked(move |_| split.set_show_sidebar(false));
        }
        hb.pack_start(&back);
        tv.add_top_bar(&hb);
        let clamp = adw::Clamp::builder().maximum_size(520).child(content).build();
        let scr = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
        tv.set_content(Some(&scr));
        (tv, wt)
    };

    let (admin_panel, _) = make_panel("Administration", &admin_box);
    let (diag_panel, _) = make_panel("Diagnostics", &diag_box);
    let (requests_panel, _) = make_panel("Join requests", &requests_box);
    let (ticket_panel, _) = make_panel("Join ticket", &ticket_box);
    let (member_panel, member_title) = make_panel("Member", &member_box);

    let stack = gtk::Stack::new();
    stack.add_named(&admin_panel, Some("admin"));
    stack.add_named(&diag_panel, Some("diagnostics"));
    stack.add_named(&requests_panel, Some("requests"));
    stack.add_named(&ticket_panel, Some("ticket"));
    stack.add_named(&member_panel, Some("member"));
    split.set_sidebar(Some(&stack));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&split));
    window.set_content(Some(&toast_overlay));

    let ui = Ui {
        split,
        stack,
        main_box,
        admin_box,
        diag_box,
        requests_box,
        ticket_box,
        member_box,
        member_title,
    };

    render_placeholder(&ui, &connecting_page());

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

    {
        let ui = ui.clone();
        let window = window.clone();
        let net = net.clone();
        let toast_overlay = toast_overlay.clone();
        let state = state.clone();
        let pending = pending.clone();
        let app_n = app.clone();
        let add_btn = add_btn.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::Status(Some(s)) => {
                        add_btn.set_visible(false); // already in a network
                        notify_newly_online(&app_n, state.borrow().as_ref(), &s);
                        pending
                            .borrow_mut()
                            .retain(|p| !s.members.iter().any(|m| m.node_id == p.node_id));
                        *state.borrow_mut() = Some(s.clone());
                        render_all(&ui, &s, &net, &window, &pending);
                    }
                    UiMsg::Status(None) => {
                        add_btn.set_visible(true); // no network — offer create/join
                        *state.borrow_mut() = None;
                        render_placeholder(&ui, &empty_page(&net, &window));
                    }
                    UiMsg::Refresh => {
                        if let Some(s) = state.borrow().as_ref() {
                            render_all(&ui, s, &net, &window, &pending);
                        }
                    }
                    UiMsg::DaemonDown => {
                        add_btn.set_visible(false);
                        *state.borrow_mut() = None;
                        render_placeholder(&ui, &daemon_down_page());
                    }
                    UiMsg::VersionMismatch { daemon, gui } => {
                        add_btn.set_visible(false);
                        *state.borrow_mut() = None;
                        render_placeholder(&ui, &version_mismatch_page(daemon, gui));
                    }
                    UiMsg::Ticket(t) => fill_ticket(&ui, &t, &net, &window),
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
                        let n = gtk::gio::Notification::new("iroh-private-network");
                        n.set_body(Some(&format!("“{hostname}” wants to join — approve in IPN")));
                        app_n.send_notification(None, &n);
                        if let Some(s) = state.borrow().as_ref() {
                            render_all(&ui, s, &net, &window, &pending);
                        }
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

    // --- system tray + minimize-to-tray ---
    let (quit_tx, quit_rx) = async_channel::unbounded::<()>();
    tray::install(app, &window, quit_tx.clone());

    {
        let action = gtk::gio::SimpleAction::new("quit", None);
        let qtx = quit_tx.clone();
        action.connect_activate(move |_, _| {
            let _ = qtx.try_send(());
        });
        app.add_action(&action);
        app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
    }

    {
        let app = app.clone();
        let notified = std::cell::Cell::new(false);
        window.connect_close_request(move |w| {
            save_window_size(w);
            w.set_visible(false);
            if !notified.replace(true) {
                let n = gtk::gio::Notification::new("iroh-private-network");
                n.set_body(Some(
                    "Still running in the tray — click the tray icon to reopen, or “Quit IPN” to disconnect.",
                ));
                app.send_notification(Some("ipn-tray"), &n);
            }
            glib::Propagation::Stop
        });
    }

    {
        let app = app.clone();
        let net = net.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            while quit_rx.recv().await.is_ok() {
                save_window_size(&window);
                let (done_tx, done_rx) = async_channel::bounded::<()>(1);
                let socket = net.socket.clone();
                net.handle.spawn(async move {
                    let _ = transport::oneshot_request(&socket, IpcRequest::Disconnect).await;
                    let _ = done_tx.send(()).await;
                });
                let _ = done_rx.recv().await;
                app.quit();
            }
        });
    }

    net.request(IpcRequest::Connect, |r| match r {
        Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
        _ => None,
    });

    if start_minimized {
        window.set_visible(false);
    } else {
        window.present();
    }
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

fn connecting_page() -> adw::StatusPage {
    let spinner = gtk::Spinner::builder().width_request(32).height_request(32).build();
    spinner.start();
    adw::StatusPage::builder()
        .title("Connecting…")
        .description("Reaching the IPN background service.")
        .css_classes(["empty-state"])
        .child(&spinner)
        .vexpand(true)
        .build()
}

fn daemon_down_page() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("network-error-symbolic")
        .title("Service not running")
        .description(
            "The privileged ipn-daemon isn't reachable. Start it (Windows: the IPN service; \
             Linux: the daemon / systemd service). This window reconnects automatically.",
        )
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
             versions. Update both IPN components to the same release."
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
    adw::StatusPage::builder()
        .icon_name("network-workgroup-symbolic")
        .title("No network yet")
        .description("Create a private network for your own devices, or join one with a ticket.")
        .css_classes(["empty-state"])
        .child(&buttons)
        .vexpand(true)
        .build()
}

/// Render the main page and the (persistent) flyout content boxes.
fn render_all(
    ui: &Ui,
    s: &NetworkStatus,
    net: &Net,
    window: &adw::ApplicationWindow,
    pending: &Rc<RefCell<Vec<PendingJoin>>>,
) {
    render_main(ui, s, net, window, pending);
    render_admin(&ui.admin_box, s, net, window);
    render_diag(&ui.diag_box, s);
    render_requests(&ui.requests_box, net, pending);
}

/// Build the main page: connection banners, the control group (Administration,
/// Show join ticket, Diagnostics), and the Members list (this device included).
fn render_main(
    ui: &Ui,
    s: &NetworkStatus,
    net: &Net,
    window: &adw::ApplicationWindow,
    pending: &Rc<RefCell<Vec<PendingJoin>>>,
) {
    clear_box(&ui.main_box);

    if !s.online {
        ui.main_box.append(
            &adw::Banner::builder()
                .title("Disconnected — reopen the app to reconnect")
                .revealed(true)
                .build(),
        );
    } else if !s.routing {
        ui.main_box.append(
            &adw::Banner::builder()
                .title("Routing off — start the daemon elevated to carry traffic")
                .revealed(true)
                .build(),
        );
    }

    // Control group: Administration (top) → Show join ticket → Diagnostics
    // (bottom), with Join requests surfaced above when present.
    let controls = adw::PreferencesGroup::new();
    let n_pending = pending.borrow().len();
    if n_pending > 0 {
        let row = flyout_row("Join requests", &format!("{n_pending} waiting"), "dialog-question-symbolic");
        let ui2 = ui.clone();
        row.connect_activated(move |_| ui2.open("requests"));
        controls.add(&row);
    }
    {
        let row = flyout_row("Administration", "Name, freeze, rotate, recovery, delete/leave", "emblem-system-symbolic");
        let ui2 = ui.clone();
        row.connect_activated(move |_| ui2.open("admin"));
        controls.add(&row);
    }
    {
        let row = flyout_row("Show join ticket", "Invite another device", "send-to-symbolic");
        let net2 = net.clone();
        row.connect_activated(move |_| {
            net2.request(IpcRequest::GetTicket, |r| match r {
                Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
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
        // About opens the standard dialog (not a flyout).
        let row = flyout_row("About IPN", &format!("Version {}", env!("CARGO_PKG_VERSION")), "help-about-symbolic");
        let window2 = window.clone();
        row.connect_activated(move |_| show_about(&window2));
        controls.add(&row);
    }
    ui.main_box.append(&controls);

    // Members (this device first), each row opens a detail flyout.
    let others = s.members.iter().filter(|m| !m.is_self).count();
    let members = adw::PreferencesGroup::builder()
        .title("Members")
        .description(format!("{} device(s) total", others + 1))
        .build();
    let mut ordered: Vec<&MemberView> = s.members.iter().collect();
    ordered.sort_by_key(|m| !m.is_self); // self first
    for m in ordered {
        members.add(&member_row(ui, m, s.is_originator, net, window));
    }
    ui.main_box.append(&members);
}

/// One member row for the main list (dot + name/host/ip/status + chevron).
fn member_row(
    ui: &Ui,
    m: &MemberView,
    is_originator: bool,
    net: &Net,
    window: &adw::ApplicationWindow,
) -> adw::ActionRow {
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("status-dot");
    dot.add_css_class(if m.online { "success" } else { "dim-label" });
    dot.set_valign(gtk::Align::Center);
    dot.set_tooltip_text(Some(if m.online { "Online" } else { "Offline" }));

    let mut title = m
        .label
        .clone()
        .or_else(|| m.hostname.clone())
        .unwrap_or_else(|| short_id(&m.node_id));
    if m.is_self {
        title.push_str(" (this device)");
    }

    let mut subtitle = String::new();
    if m.label.is_some() {
        if let Some(h) = &m.hostname {
            subtitle.push_str(h);
            subtitle.push_str(" · ");
        }
    }
    subtitle.push_str(&m.virtual_ip.clone().unwrap_or_else(|| "(no IP)".into()));
    if m.is_self {
        // (online by definition)
    } else if m.online {
        match m.direct {
            Some(true) => subtitle.push_str(" · direct"),
            Some(false) => subtitle.push_str(" · relay"),
            None => {}
        }
    } else {
        subtitle.push_str(&format!(" · last seen {}", fmt_last_seen(m.last_seen)));
    }

    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_prefix(&dot);
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let ui2 = ui.clone();
    let net2 = net.clone();
    let window2 = window.clone();
    let m2 = m.clone();
    row.connect_activated(move |_| fill_member(&ui2, &m2, is_originator, &net2, &window2));
    row
}

fn render_diag(b: &gtk::Box, s: &NetworkStatus) {
    clear_box(b);
    let g = adw::PreferencesGroup::new();
    g.add(&property_row("Home relay", &s.home_relay.clone().unwrap_or_else(|| "—".into())));
    let direct = s.members.iter().filter(|m| !m.is_self && m.online && m.direct == Some(true)).count();
    let relayed = s.members.iter().filter(|m| !m.is_self && m.online && m.direct == Some(false)).count();
    g.add(&property_row("Connections", &format!("{direct} direct · {relayed} via relay")));
    g.add(&property_row(
        "Routing (TUN)",
        if s.routing { "on — carrying traffic" } else { "off — needs the elevated daemon" },
    ));
    b.append(&g);
}

fn render_admin(b: &gtk::Box, s: &NetworkStatus, net: &Net, window: &adw::ApplicationWindow) {
    clear_box(b);

    // Network name (rename here, not on the main screen).
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

fn render_requests(b: &gtk::Box, net: &Net, pending: &Rc<RefCell<Vec<PendingJoin>>>) {
    clear_box(b);
    let plist = pending.borrow();
    if plist.is_empty() {
        b.append(
            &adw::StatusPage::builder()
                .icon_name("dialog-question-symbolic")
                .title("No pending requests")
                .css_classes(["empty-state"])
                .vexpand(true)
                .build(),
        );
        return;
    }
    let g = adw::PreferencesGroup::builder()
        .description("Approve only if the emoji code matches the joining device's screen.")
        .build();
    for req in plist.iter() {
        let row = adw::ActionRow::builder()
            .title(format!("“{}” wants to join", req.hostname))
            .subtitle(req.sas.join("  "))
            .build();
        let deny = gtk::Button::builder().label("Deny").valign(gtk::Align::Center).build();
        deny.add_css_class("flat");
        let approve = gtk::Button::builder().label("Approve").valign(gtk::Align::Center).build();
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
        row.add_suffix(&deny);
        row.add_suffix(&approve);
        g.add(&row);
    }
    b.append(&g);
}

/// Fill + open the per-member detail flyout.
fn fill_member(
    ui: &Ui,
    m: &MemberView,
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

    if m.is_self {
        let name_row = adw::ActionRow::builder()
            .title("Friendly name")
            .subtitle(m.label.clone().unwrap_or_else(|| "(none)".into()))
            .build();
        let edit = icon_button("document-edit-symbolic", "Set this device's friendly name");
        let window2 = window.clone();
        let net2 = net.clone();
        let cur = m.label.clone();
        edit.connect_clicked(move |_| set_label_dialog(&window2, &net2, cur.clone()));
        name_row.add_suffix(&edit);
        g.add(&name_row);
    } else if let Some(l) = &m.label {
        g.add(&property_row("Friendly name", l));
    }

    g.add(&property_row("Hostname", &m.hostname.clone().unwrap_or_else(|| "—".into())));
    g.add(&property_row("Status", if m.online { "Online" } else { "Offline" }));
    if !m.is_self && !m.online {
        g.add(&property_row("Last seen", &fmt_last_seen(m.last_seen)));
    }
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
    if !m.is_self && m.online {
        g.add(&property_row(
            "Connection",
            match m.direct {
                Some(true) => "Direct (peer-to-peer)",
                Some(false) => "Via relay",
                None => "—",
            },
        ));
    }
    if let Some(addr) = &m.observed_addr {
        g.add(&property_row("Observed address", addr));
    }
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
    ui.member_box.append(&g);

    if !m.is_self && is_originator {
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
    let entry = gtk::Entry::builder().placeholder_text("ipn1...").build();
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
        if !ticket.trim().starts_with("ipn1") {
            net.toast("That doesn't look like a join ticket (it should start with “ipn1…”).");
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

fn set_label_dialog(window: &adw::ApplicationWindow, net: &Net, current: Option<String>) {
    let entry = gtk::Entry::builder()
        .text(current.unwrap_or_default())
        .placeholder_text("Friendly name (leave blank to clear)")
        .build();
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Set this device's name")
        .body(
            "A friendly label other members see. The hostname (your real OS name) is always \
             shown too and can't be changed here.",
        )
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
        let text = entry.text().to_string();
        let label = if text.trim().is_empty() { None } else { Some(text) };
        net.request(IpcRequest::SetLabel { label }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Name updated".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
}

fn import_originator_dialog(window: &adw::ApplicationWindow, net: &Net) {
    let entry = gtk::Entry::builder().placeholder_text("ipnkey1...").build();
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
    let about = adw::AboutWindow::builder()
        .transient_for(window)
        .application_name("iroh-private-network")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("steeb_k")
        .license_type(gtk::License::Gpl30)
        .comments("A peer-to-peer private VPN over iroh — connect your own devices into a private LAN.")
        .build();
    about.present();
}

/// Notify when a member transitions offline→online (skips the first render).
fn notify_newly_online(app: &adw::Application, prev: Option<&NetworkStatus>, new: &NetworkStatus) {
    let Some(prev) = prev else { return };
    for m in &new.members {
        if m.is_self || !m.online {
            continue;
        }
        let was_online = prev.members.iter().any(|p| p.node_id == m.node_id && p.online);
        if !was_online {
            let name = m
                .label
                .clone()
                .or_else(|| m.hostname.clone())
                .unwrap_or_else(|| short_id(&m.node_id));
            let n = gtk::gio::Notification::new("iroh-private-network");
            n.set_body(Some(&format!("{name} came online")));
            app.send_notification(None, &n);
        }
    }
}

/// A large, centered emoji label for the SAS.
fn sas_label(sas: &[String]) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_markup(&format!(
        "<span size='350%'>{}</span>",
        glib::markup_escape_text(&sas.join("  "))
    ));
    label.set_justify(gtk::Justification::Center);
    label.set_halign(gtk::Align::Center);
    label.set_wrap(true);
    label
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
