//! iroh-private-network desktop GUI (GTK4 + libadwaita) — an **unprivileged IPC
//! client** to `ipn-daemon`. The daemon owns the iroh node + TUN (the only thing
//! needing elevation); this process just renders state and sends commands, so it
//! never needs admin/root.
//!
//! Threading: a Tokio runtime on a side thread does the socket IO; results and
//! pushed events arrive on the GTK main thread via an `async-channel` consumed by
//! `glib::spawn_future_local`. GTK objects are only touched on the main thread.

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;
use ipn_ipc::transport::{self, read_frame, write_frame};
use ipn_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message, NetworkStatus};
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
    Toast(String),
    DaemonDown,
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

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Tokio runtime on a dedicated thread for socket IO.
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
    app.connect_activate(move |app| build_ui(app, net.clone(), rx.clone()));
    let empty: [&str; 0] = [];
    app.run_with_args(&empty)
}

fn build_ui(app: &adw::Application, net: Net, rx: async_channel::Receiver<UiMsg>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("iroh-private-network")
        .default_width(560)
        .default_height(640)
        .build();

    let header = adw::HeaderBar::new();
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
    header.pack_start(&add_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    let clamp = adw::Clamp::builder().maximum_size(520).child(&content).build();
    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
    toolbar.set_content(Some(&scrolled));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));
    window.set_content(Some(&toast_overlay));

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

    {
        let content = content.clone();
        let window = window.clone();
        let net = net.clone();
        let toast_overlay = toast_overlay.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::Status(Some(s)) => render_status(&content, &s, &net, &window),
                    UiMsg::Status(None) => render_empty(&content),
                    UiMsg::DaemonDown => render_daemon_down(&content),
                    UiMsg::Ticket(t) => show_ticket(&window, &t),
                    UiMsg::JoinSas(sas) => show_join_sas(&window, &sas),
                    UiMsg::JoinRequest {
                        node_id,
                        hostname,
                        sas,
                    } => show_join_request(&window, &net, &node_id, &hostname, &sas),
                    UiMsg::Toast(t) => toast_overlay.add_toast(adw::Toast::new(&t)),
                }
            }
        });
    }

    // --- system tray + minimize-to-tray ---
    let (quit_tx, quit_rx) = async_channel::unbounded::<()>();
    tray::install(app, &window, quit_tx);

    // Closing the window hides it to the tray (keeps the connection) and, the first
    // time, notifies the user it's still running.
    {
        let app = app.clone();
        let notified = std::cell::Cell::new(false);
        window.connect_close_request(move |w| {
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

    // "Quit IPN" from the tray: disconnect from the network locally, then exit.
    {
        let app = app.clone();
        let net = net.clone();
        glib::spawn_future_local(async move {
            while quit_rx.recv().await.is_ok() {
                let (done_tx, done_rx) = async_channel::bounded::<()>(1);
                let socket = net.socket.clone();
                net.handle.spawn(async move {
                    let _ = transport::oneshot_request(&socket, IpcRequest::Disconnect).await;
                    let _ = done_tx.send(()).await;
                });
                let _ = done_rx.recv().await; // wait for the disconnect to land
                app.quit();
            }
        });
    }

    // Opening the app connects to the saved network (reconnects if a prior "Quit"
    // left it offline).
    net.request(IpcRequest::Connect, |r| match r {
        Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
        _ => None,
    });

    window.present();
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

fn render_daemon_down(content: &gtk::Box) {
    clear_box(content);
    let status = adw::StatusPage::builder()
        .icon_name("network-error-symbolic")
        .title("Daemon not running")
        .description(
            "The privileged ipn-daemon isn't reachable. Start it (Windows: the IPN service; \
             Linux: `sudo ipn-daemon` or the systemd service). This window reconnects automatically.",
        )
        .vexpand(true)
        .build();
    content.append(&status);
}

fn render_empty(content: &gtk::Box) {
    clear_box(content);
    let status = adw::StatusPage::builder()
        .icon_name("network-workgroup-symbolic")
        .title("No network yet")
        .description("Click + to create a private network, or join one with a ticket.")
        .vexpand(true)
        .build();
    content.append(&status);
}

fn render_status(content: &gtk::Box, s: &NetworkStatus, net: &Net, window: &adw::ApplicationWindow) {
    clear_box(content);

    if !s.online {
        let banner = adw::Banner::builder()
            .title("Disconnected — reopen the app to reconnect")
            .revealed(true)
            .build();
        content.append(&banner);
    } else if !s.routing {
        let banner = adw::Banner::builder()
            .title("Routing off — start the daemon elevated to carry RDP/SSH traffic")
            .revealed(true)
            .build();
        content.append(&banner);
    }

    let info = adw::PreferencesGroup::builder().title(&s.name).build();
    let self_row = adw::ActionRow::builder()
        .title("This device")
        .subtitle(format!(
            "{}{} · routing {}",
            s.self_ip.clone().unwrap_or_else(|| "(no IP yet)".into()),
            if s.is_originator { " · originator" } else { "" },
            if s.routing { "on" } else { "off" }
        ))
        .build();
    info.add(&self_row);

    if s.is_originator {
        let freeze = gtk::Switch::builder()
            .active(s.frozen)
            .valign(gtk::Align::Center)
            .tooltip_text("Freeze membership (no new devices can join)")
            .build();
        let net2 = net.clone();
        freeze.connect_state_set(move |_, state| {
            net2.request(IpcRequest::SetFrozen { frozen: state }, |r| match r {
                Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                _ => None,
            });
            glib::Propagation::Proceed
        });
        let frow = adw::ActionRow::builder().title("Membership frozen").build();
        frow.add_suffix(&freeze);
        info.add(&frow);
    }
    content.append(&info);

    let group = adw::PreferencesGroup::builder()
        .title("Members")
        .description(format!("{} member(s)", s.members.len()))
        .build();
    for m in &s.members {
        if m.is_self {
            continue;
        }
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class(if m.online { "success" } else { "dim-label" });
        dot.set_valign(gtk::Align::Center);

        let mut subtitle = m.virtual_ip.clone().unwrap_or_else(|| "(no IP)".into());
        if let Some(addr) = &m.observed_addr {
            subtitle.push_str(" · ");
            subtitle.push_str(addr);
        }
        match m.direct {
            Some(true) => subtitle.push_str(" · direct"),
            Some(false) => subtitle.push_str(" · relay"),
            None => {}
        }
        if !m.online {
            subtitle.push_str(&format!(" · last seen {}", fmt_last_seen(m.last_seen)));
        }

        let row = adw::ActionRow::builder()
            .title(m.hostname.clone().unwrap_or_else(|| short_id(&m.node_id)))
            .subtitle(subtitle)
            .build();
        row.add_prefix(&dot);

        if let Some(ip) = &m.virtual_ip {
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy virtual IP")
                .valign(gtk::Align::Center)
                .build();
            copy.add_css_class("flat");
            let ip = ip.clone();
            let win = window.clone();
            copy.connect_clicked(move |_| {
                win.clipboard().set_text(&ip);
            });
            row.add_suffix(&copy);
        }

        if s.is_originator {
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove this member")
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            let net2 = net.clone();
            let id = m.node_id.clone();
            remove.connect_clicked(move |_| {
                net2.request(IpcRequest::RemoveMember { node_id: id.clone() }, |r| match r {
                    Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
                    _ => None,
                });
            });
            row.add_suffix(&remove);
        }

        group.add(&row);
    }
    content.append(&group);

    let share = gtk::Button::builder()
        .label("Show join ticket")
        .halign(gtk::Align::Center)
        .build();
    share.add_css_class("pill");
    let net2 = net.clone();
    share.connect_clicked(move |_| {
        net2.request(IpcRequest::GetTicket, |r| match r {
            Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    content.append(&share);

    // Originator: rotate the secret (mass-revoke + re-key).
    if s.is_originator {
        let rotate = gtk::Button::builder()
            .label("Rotate secret (re-key)")
            .halign(gtk::Align::Center)
            .margin_top(8)
            .build();
        let net2 = net.clone();
        let window2 = window.clone();
        rotate.connect_clicked(move |_| confirm_rotate(&window2, &net2));
        content.append(&rotate);
    }

    // Danger zone: originator can dissolve the whole network; anyone else can leave.
    let danger = gtk::Button::builder()
        .label(if s.is_originator {
            "Delete network"
        } else {
            "Leave network"
        })
        .halign(gtk::Align::Center)
        .margin_top(8)
        .build();
    danger.add_css_class("destructive-action");
    let net2 = net.clone();
    let window2 = window.clone();
    let is_orig = s.is_originator;
    danger.connect_clicked(move |_| {
        confirm_destroy(&window2, &net2, is_orig);
    });
    content.append(&danger);
}

/// Confirm dialog for rotating the network secret (mass-revoke + re-key).
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

/// Confirm dialog for deleting (originator) or leaving (member) the network.
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

// --- dialogs ---

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
            Ok(IpcResponse::Ticket(t)) => Some(UiMsg::Ticket(t)),
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
        net.request(IpcRequest::Join { ticket }, |r| match r {
            Ok(IpcResponse::Ok) => Some(UiMsg::Toast("Joined!".into())),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("join failed: {e}"))),
            Err(_) => Some(UiMsg::DaemonDown),
            _ => None,
        });
    });
    dialog.present();
}

/// Render the ticket as a fixed-size QR image (a `gtk::Picture`), ~240px.
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

fn show_ticket(window: &adw::ApplicationWindow, ticket: &str) {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);

    if let Some(pic) = qr_picture(ticket) {
        vbox.append(&pic);
    }

    // The key in a compact, scrollable single-line box + a copy button beside it.
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder()
        .text(ticket)
        .editable(false)
        .hexpand(true)
        .build();
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy ticket"));
    copy.set_valign(gtk::Align::Center);
    let ticket_owned = ticket.to_string();
    let win = window.clone();
    copy.connect_clicked(move |_| {
        win.clipboard().set_text(&ticket_owned);
    });
    row.append(&entry);
    row.append(&copy);
    vbox.append(&row);

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading("Join ticket")
        .body("Scan the QR from the other device, or copy the ticket and paste it into Join.")
        .extra_child(&vbox)
        .build();
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.present();
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

fn show_join_request(
    window: &adw::ApplicationWindow,
    net: &Net,
    node_id: &str,
    hostname: &str,
    sas: &[String],
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading(format!("“{hostname}” wants to join"))
        .body("Approve only if these emojis match the joining device's screen.")
        .extra_child(&sas_label(sas))
        .build();
    dialog.add_response("deny", "Deny");
    dialog.add_response("approve", "Approve");
    dialog.set_response_appearance("approve", adw::ResponseAppearance::Suggested);
    let net = net.clone();
    let node_id = node_id.to_string();
    dialog.connect_response(None, move |_, resp| {
        let req = if resp == "approve" {
            IpcRequest::ApproveJoin {
                node_id: node_id.clone(),
            }
        } else {
            IpcRequest::DenyJoin {
                node_id: node_id.clone(),
            }
        };
        net.request(req, |r| match r {
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(e)),
            _ => None,
        });
    });
    dialog.present();
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
